// CleanRamVPS — Rust RAM cleaner với GUI nhẹ
// 4 chế độ: EmptyWorkingSets, FlushModifiedList, PurgeStandbyList, PurgeLowPriorityStandbyList
// 8 mức interval cho auto-loop: 5s, 30s, 1p, 2p, 5p, 10p, 15p, 30p
//
// Build:
//   cargo build --release
// Output:
//   target/release/CleanRamVPS.exe  (Windows)
//
// Lưu ý: cần chạy với quyền Administrator để NtSetSystemInformation thành công.
// Nếu build cho Windows từ máy khác, thêm target:
//   rustup target add x86_64-pc-windows-gnu
//   cargo build --release --target x86_64-pc-windows-gnu

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::System;

// -----------------------------------------------------------------------
// Windows syscall layer
// -----------------------------------------------------------------------
#[cfg(windows)]
mod winmem {
    use windows_sys::Win32::Foundation::NTSTATUS;
    use std::ffi::c_void;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtSetSystemInformation(
            system_information_class: i32,
            system_information: *mut c_void,
            system_information_length: u32,
        ) -> NTSTATUS;
    }

    const SYSTEM_MEMORY_LIST_INFORMATION: i32 = 0x50;

    #[repr(i32)]
    #[derive(Clone, Copy)]
    pub enum MemoryListCommand {
        EmptyWorkingSets = 2,
        FlushModifiedList = 3,
        PurgeStandbyList = 4,
        PurgeLowPriorityStandbyList = 5,
    }

    /// Gọi NtSetSystemInformation với command tương ứng.
    /// Trả về Ok(()) nếu NTSTATUS == 0, ngược lại Err(mã lỗi hex).
    pub fn run_command(cmd: MemoryListCommand) -> Result<(), String> {
        let mut value: i32 = cmd as i32;
        let status = unsafe {
            NtSetSystemInformation(
                SYSTEM_MEMORY_LIST_INFORMATION,
                &mut value as *mut _ as *mut c_void,
                std::mem::size_of::<i32>() as u32,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("NTSTATUS 0x{:08X} (có thể thiếu quyền Administrator)", status as u32))
        }
    }
}

#[cfg(not(windows))]
mod winmem {
    #[derive(Clone, Copy)]
    pub enum MemoryListCommand {
        EmptyWorkingSets,
        FlushModifiedList,
        PurgeStandbyList,
        PurgeLowPriorityStandbyList,
    }
    pub fn run_command(_cmd: MemoryListCommand) -> Result<(), String> {
        Err("Chỉ hỗ trợ Windows.".to_string())
    }
}

use winmem::MemoryListCommand;

// -----------------------------------------------------------------------
// Chế độ & interval
// -----------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    EmptyWorkingSets,
    FlushModifiedList,
    PurgeStandbyList,
    PurgeLowPriorityStandbyList,
}

impl Mode {
    const ALL: [Mode; 4] = [
        Mode::EmptyWorkingSets,
        Mode::FlushModifiedList,
        Mode::PurgeStandbyList,
        Mode::PurgeLowPriorityStandbyList,
    ];

    fn label(&self) -> &'static str {
        match self {
            Mode::EmptyWorkingSets => "Empty Working Sets",
            Mode::FlushModifiedList => "Flush Modified List",
            Mode::PurgeStandbyList => "Purge Standby List",
            Mode::PurgeLowPriorityStandbyList => "Purge Low-Priority Standby List",
        }
    }

    /// Chú thích tiếng Việt giải thích chế độ này làm gì, hiển thị dưới dropdown.
    fn description_vi(&self) -> &'static str {
        match self {
            Mode::EmptyWorkingSets =>
                "Đẩy bộ nhớ (working set) của tất cả tiến trình đang chạy về \
                 bộ nhớ đệm hệ thống. Không giảm RAM tổng thể ngay lập tức, \
                 chỉ 'gọn' lại bộ nhớ riêng của từng app.",
            Mode::FlushModifiedList =>
                "Ghi các trang nhớ 'đã sửa nhưng chưa lưu' (modified pages) \
                 xuống ổ đĩa/pagefile, giúp giải phóng chúng khỏi RAM.",
            Mode::PurgeStandbyList =>
                "Xóa toàn bộ bộ nhớ đệm hệ thống (standby list / cache file). \
                 Đây là phần Windows hiển thị là 'Cached' trong Task Manager. \
                 Giảm RAM 'used' rõ rệt nhất, nhưng có thể làm app mở lại chậm hơn.",
            Mode::PurgeLowPriorityStandbyList =>
                "Chỉ xóa phần cache ưu tiên thấp (ít dùng gần đây), giữ lại \
                 cache quan trọng. Nhẹ nhàng hơn Purge Standby List toàn bộ.",
        }
    }

    fn to_command(self) -> MemoryListCommand {
        match self {
            Mode::EmptyWorkingSets => MemoryListCommand::EmptyWorkingSets,
            Mode::FlushModifiedList => MemoryListCommand::FlushModifiedList,
            Mode::PurgeStandbyList => MemoryListCommand::PurgeStandbyList,
            Mode::PurgeLowPriorityStandbyList => MemoryListCommand::PurgeLowPriorityStandbyList,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct IntervalOpt {
    secs: u64,
    label: &'static str,
}

const INTERVALS: [IntervalOpt; 8] = [
    IntervalOpt { secs: 5, label: "5 giây" },
    IntervalOpt { secs: 30, label: "30 giây" },
    IntervalOpt { secs: 60, label: "1 phút" },
    IntervalOpt { secs: 120, label: "2 phút" },
    IntervalOpt { secs: 300, label: "5 phút" },
    IntervalOpt { secs: 600, label: "10 phút" },
    IntervalOpt { secs: 900, label: "15 phút" },
    IntervalOpt { secs: 1800, label: "30 phút" },
];

// -----------------------------------------------------------------------
// System monitor: chạy 1 thread nền riêng, đọc RAM/CPU mỗi giây
// -----------------------------------------------------------------------
#[derive(Clone, Copy, Default)]
struct SysSnapshot {
    ram_used_mb: u64,
    ram_total_mb: u64,
    ram_percent: f32,
    cpu_percent: f32,
}

struct SysMonitor {
    snapshot: Arc<Mutex<SysSnapshot>>,
    running: Arc<AtomicBool>,
    _handle: thread::JoinHandle<()>,
    start_time: Instant,
    // tổng số giây "app đang thức" (đơn giản = từ lúc mở app, vì không cần track sleep OS)
    uptime_secs: Arc<AtomicU64>,
}

impl SysMonitor {
    fn start() -> Self {
        let snapshot = Arc::new(Mutex::new(SysSnapshot::default()));
        let running = Arc::new(AtomicBool::new(true));
        let uptime_secs = Arc::new(AtomicU64::new(0));

        let snap_clone = Arc::clone(&snapshot);
        let running_clone = Arc::clone(&running);
        let uptime_clone = Arc::clone(&uptime_secs);

        let handle = thread::spawn(move || {
            let mut sys = System::new_all();
            // sysinfo cần 2 lần refresh cách nhau 1 khoảng để tính %CPU chính xác
            sys.refresh_cpu_usage();
            thread::sleep(Duration::from_millis(200));

            while running_clone.load(Ordering::SeqCst) {
                sys.refresh_memory();
                sys.refresh_cpu_usage();

                let total_kb = sys.total_memory(); // KB
                let used_kb = sys.used_memory();
                let ram_total_mb = total_kb / 1024;
                let ram_used_mb = used_kb / 1024;
                let ram_percent = if total_kb > 0 {
                    (used_kb as f32 / total_kb as f32) * 100.0
                } else {
                    0.0
                };
                let cpu_percent = sys.global_cpu_usage();

                if let Ok(mut s) = snap_clone.lock() {
                    *s = SysSnapshot {
                        ram_used_mb,
                        ram_total_mb,
                        ram_percent,
                        cpu_percent,
                    };
                }

                uptime_clone.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_secs(1));
            }
        });

        Self {
            snapshot,
            running,
            _handle: handle,
            start_time: Instant::now(),
            uptime_secs,
        }
    }

    fn read(&self) -> SysSnapshot {
        self.snapshot.lock().map(|s| *s).unwrap_or_default()
    }

    fn uptime_string(&self) -> String {
        let secs = self.uptime_secs.load(Ordering::SeqCst);
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{h}h {m}p {s}s")
        } else if m > 0 {
            format!("{m}p {s}s")
        } else {
            format!("{s}s")
        }
    }
}

impl Drop for SysMonitor {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

// -----------------------------------------------------------------------
// App state
// -----------------------------------------------------------------------
struct CleanRamApp {
    mode: Mode,
    interval_idx: usize,
    status: String,
    looping: Arc<AtomicBool>,
    loop_handle: Option<thread::JoinHandle<()>>,
    last_result_ok: Option<bool>,
    monitor: SysMonitor,
}

impl Default for CleanRamApp {
    fn default() -> Self {
        Self {
            mode: Mode::EmptyWorkingSets,
            interval_idx: 0, // mặc định 5s
            status: "Sẵn sàng.".to_string(),
            looping: Arc::new(AtomicBool::new(false)),
            loop_handle: None,
            last_result_ok: None,
            monitor: SysMonitor::start(),
        }
    }
}

impl CleanRamApp {
    fn clean_once(&mut self) {
        match winmem::run_command(self.mode.to_command()) {
            Ok(()) => {
                self.status = format!("✅ {} thành công.", self.mode.label());
                self.last_result_ok = Some(true);
            }
            Err(e) => {
                self.status = format!("❌ Lỗi: {e}");
                self.last_result_ok = Some(false);
            }
        }
    }

    fn start_loop(&mut self) {
        if self.looping.load(Ordering::SeqCst) {
            return;
        }
        self.looping.store(true, Ordering::SeqCst);
        let flag = Arc::clone(&self.looping);
        let mode = self.mode;
        let secs = INTERVALS[self.interval_idx].secs;

        let handle = thread::spawn(move || {
            while flag.load(Ordering::SeqCst) {
                let _ = winmem::run_command(mode.to_command());
                // Ngủ theo từng giây để phản hồi nhanh khi bấm Stop
                for _ in 0..secs {
                    if !flag.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
        });
        self.loop_handle = Some(handle);
        self.status = format!(
            "▶ Auto-clean đang chạy mỗi {} ({})",
            INTERVALS[self.interval_idx].label,
            self.mode.label()
        );
    }

    fn stop_loop(&mut self) {
        self.looping.store(false, Ordering::SeqCst);
        if let Some(h) = self.loop_handle.take() {
            let _ = h.join();
        }
        self.status = "⏹ Auto-clean đã dừng.".to_string();
    }
}

impl eframe::App for CleanRamApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Luôn repaint mỗi giây để RAM/CPU/uptime cập nhật liên tục,
        // kể cả khi không bật auto-clean.
        ctx.request_repaint_after(Duration::from_millis(1000));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("🧹 Toonie Clean Ram");
            ui.small("by Toonie (Tú Nguyễn)");
            ui.separator();

            // ---------------- System monitor ----------------
            let snap = self.monitor.read();
            ui.label(format!(
                "RAM: {} / {} MB  ({:.1}%)",
                snap.ram_used_mb, snap.ram_total_mb, snap.ram_percent
            ));
            ui.add(
                egui::ProgressBar::new((snap.ram_percent / 100.0).clamp(0.0, 1.0))
                    .text(format!("{:.1}%", snap.ram_percent)),
            );

            ui.add_space(4.0);
            ui.label(format!("CPU: {:.1}%", snap.cpu_percent));
            ui.add(
                egui::ProgressBar::new((snap.cpu_percent / 100.0).clamp(0.0, 1.0))
                    .text(format!("{:.1}%", snap.cpu_percent)),
            );

            ui.add_space(4.0);
            ui.small(format!("Thời gian thức (uptime app): {}", self.monitor.uptime_string()));

            ui.add_space(10.0);
            ui.separator();

            ui.label("Chế độ:");
            egui::ComboBox::from_id_source("mode_combo")
                .selected_text(self.mode.label())
                .width(300.0)
                .show_ui(ui, |ui| {
                    for m in Mode::ALL {
                        ui.selectable_value(&mut self.mode, m, m.label());
                    }
                });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(self.mode.description_vi())
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );

            ui.add_space(8.0);
            ui.label("Chu kỳ tự động:");
            egui::ComboBox::from_id_source("interval_combo")
                .selected_text(INTERVALS[self.interval_idx].label)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for (i, opt) in INTERVALS.iter().enumerate() {
                        ui.selectable_value(&mut self.interval_idx, i, opt.label);
                    }
                });

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if ui.button("🧹  Clean ngay").clicked() {
                    self.clean_once();
                }

                let is_looping = self.looping.load(Ordering::SeqCst);
                let loop_btn_label = if is_looping { "⏹ Dừng Auto" } else { "▶ Bật Auto" };
                if ui.button(loop_btn_label).clicked() {
                    if is_looping {
                        self.stop_loop();
                    } else {
                        self.start_loop();
                    }
                }
            });

            ui.add_space(12.0);
            ui.separator();
            let color = match self.last_result_ok {
                Some(true) => egui::Color32::from_rgb(60, 180, 75),
                Some(false) => egui::Color32::from_rgb(220, 60, 60),
                None => ui.visuals().text_color(),
            };
            ui.colored_label(color, &self.status);

            ui.add_space(4.0);
            ui.small("Cần chạy với quyền Administrator để hoạt động.");
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.looping.store(false, Ordering::SeqCst);
        if let Some(h) = self.loop_handle.take() {
            let _ = h.join();
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([380.0, 520.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "Toonie Clean Ram",
        options,
        Box::new(|_cc| Box::new(CleanRamApp::default())),
    )
}
