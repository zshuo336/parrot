//! Resolve installed gateway resources before opening listeners or data files.
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
};

pub(crate) struct Runtime {
    pub root: PathBuf,
    pub classpath: OsString,
}

fn select(explicit: Option<&Path>, executable: &Path, development: &Path) -> PathBuf {
    if let Some(root) = explicit {
        return root.to_owned();
    }
    if let Some(release) = executable.parent().and_then(Path::parent) {
        let installed = release.join("runtime");
        // An installed but damaged release must not silently use build-machine
        // resources. The development fallback only applies outside a release.
        if installed.is_dir()
            || release.join("MANIFEST.json").is_file()
            || executable
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "bin")
        {
            return installed;
        }
    }
    development.to_owned()
}

fn regular(path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("missing host runtime file: {}", path.display()))
    }
}

impl Runtime {
    pub fn discover(explicit: Option<&Path>, crawl: bool) -> Result<Self, String> {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let development = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Self::load(&select(explicit, &executable, &development), crawl)
    }

    fn load(root: &Path, crawl: bool) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("host runtime {}: {e}", root.display()))?;
        regular(&root.join("tools/parrot-obs/gateway_worker.py"))?;
        let target = root.join("interop/jvm/target");
        let gateway = target.join("parrot-protocol-jvm-0.1.0.jar");
        regular(&gateway)?;
        let file = std::fs::File::open(target.join("cp.txt"))
            .map_err(|e| format!("host JVM classpath: {e}"))?;
        let mut content = String::new();
        file.take(1024 * 1024 + 1)
            .read_to_string(&mut content)
            .map_err(|e| format!("host JVM classpath: {e}"))?;
        if content.len() > 1024 * 1024 || content.trim().is_empty() {
            return Err("host JVM classpath is empty or exceeds byte budget".into());
        }
        let mut entries = vec![gateway];
        for entry in std::env::split_paths(content.trim()) {
            if entry.as_os_str().is_empty() {
                return Err("host JVM classpath has an empty entry".into());
            }
            let path = if entry.is_absolute() {
                entry
            } else {
                target.join(entry)
            };
            regular(&path)?;
            entries.push(path);
        }
        let classpath =
            std::env::join_paths(entries).map_err(|e| format!("host JVM classpath: {e}"))?;
        if crawl {
            for name in [
                "tools/parrot-obs/ray_gateway.py",
                "interop/python/parrot_protocol/__init__.py",
                "interop/python/parrot_protocol/ray_gw.py",
            ] {
                regular(&root.join(name))?;
            }
            for name in [
                "parrot_gw",
                "parrot_trace",
                "parrot_component_runtime",
                "parrot_beam_snapshot",
                "parrot_beam_guest",
                "parrot_beam_host",
            ] {
                regular(&root.join(format!("interop/erlang/{name}.erl")))?;
            }
        }
        Ok(Self { root, classpath })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Use an owned directory without a new production dependency.
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut bytes = [0; 16];
            getrandom::getrandom(&mut bytes).unwrap();
            let path = std::env::temp_dir()
                .join(format!("ws-host-runtime-{:x}", u128::from_ne_bytes(bytes)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn put(&self, name: &str, content: &str) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn installed_missing_runtime_never_falls_back_to_the_build_checkout() {
        let release = Directory::new();
        release.put("MANIFEST.json", "{}");
        let executable = release.0.join("bin/websearch");
        assert_eq!(
            select(None, &executable, Path::new("/build")),
            release.0.join("runtime")
        );
        assert!(Runtime::load(&release.0.join("runtime"), false).is_err());
        std::fs::remove_file(release.0.join("MANIFEST.json")).unwrap();
        assert_eq!(
            select(None, &executable, Path::new("/build")),
            release.0.join("runtime")
        );
        assert_eq!(
            select(
                Some(Path::new("/explicit")),
                &executable,
                Path::new("/build")
            ),
            PathBuf::from("/explicit")
        );
    }

    #[test]
    fn relative_classpath_uses_runtime_directory_and_preserves_dependency_order() {
        let root = Directory::new();
        root.put("tools/parrot-obs/gateway_worker.py", "");
        let target = "interop/jvm/target/";
        for name in ["parrot-protocol-jvm-0.1.0.jar", "libs/b.jar", "libs/a.jar"] {
            root.put(&format!("{target}{name}"), "");
        }
        root.put(&format!("{target}cp.txt"), "libs/b.jar:libs/a.jar\n");
        let runtime = Runtime::load(&root.0, false).unwrap();
        let paths: Vec<_> = std::env::split_paths(&runtime.classpath).collect();
        assert_eq!(
            paths
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["parrot-protocol-jvm-0.1.0.jar", "b.jar", "a.jar"]
        );
        assert!(paths.iter().all(|p| p.starts_with(&runtime.root)));
        assert!(Runtime::load(&root.0, true).is_err());
        for invalid in ["", "libs/b.jar:", "missing.jar"] {
            root.put(&format!("{target}cp.txt"), invalid);
            assert!(Runtime::load(&root.0, false).is_err());
        }
    }
}
