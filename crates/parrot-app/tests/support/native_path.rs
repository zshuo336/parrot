pub fn file_name() -> std::ffi::OsString {
    // Linux permits byte filenames; macOS filesystems reject malformed UTF-8.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"policy-\xff.toml".to_vec())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::ffi::OsString::from("policy-\u{e9}.toml")
    }
}
