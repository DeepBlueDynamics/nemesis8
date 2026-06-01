// Embed Windows PE VERSIONINFO so the firewall and Properties dialogs
// show "DeepBlue Dynamics LLC" instead of "Unknown publisher". FileVersion
// and ProductVersion default to CARGO_PKG_VERSION automatically.

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("CompanyName", "DeepBlue Dynamics LLC")
            .set("FileDescription", "nemesis8 — run AI agents in Docker")
            .set("ProductName", "nemesis8")
            .set("OriginalFilename", "nemesis8.exe")
            .set("LegalCopyright", "\u{00A9} DeepBlue Dynamics LLC");
        res.compile().expect("failed to embed Windows VERSIONINFO");
    }
}
