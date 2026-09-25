// build.rs — chạy trước khi compile, nhúng icon + thông tin version vào .exe
// Chỉ chạy trên Windows (winres chỉ hoạt động khi build target Windows).

fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();

        // Icon hiển thị trên file .exe, taskbar, shortcut...
        // Đặt file logo.ico trong thư mục gốc project (cùng cấp Cargo.toml).
        res.set_icon("logo.ico");

        // Thông tin hiện trong tab "Details" khi click phải .exe → Properties
        res.set("ProductName", "Toonie Clean Ram");
        res.set("FileDescription", "Toonie Clean Ram - Công cụ dọn RAM VPS");
        res.set("CompanyName", "Toonie (Tú Nguyễn)");
        res.set("LegalCopyright", "© Toonie (Tú Nguyễn)");
        res.set("OriginalFilename", "CleanRamVPS.exe");
        res.set("InternalName", "CleanRamVPS");
        res.set_version_info(winres::VersionInfo::PRODUCTVERSION, 1_00_00_0000);
        res.set_version_info(winres::VersionInfo::FILEVERSION, 1_00_00_0000);

        res.compile().expect(
            "Không nhúng được icon/version info. Kiểm tra file logo.ico có tồn tại không.",
        );
    }
}
