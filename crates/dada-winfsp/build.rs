//! Links WinFsp with delay loading, so the binary starts and reports a clear
//! error when WinFsp is not installed.

fn main() {
    #[cfg(all(windows, feature = "winfsp"))]
    winfsp::build::winfsp_link_delayload();
}
