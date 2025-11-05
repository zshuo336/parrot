//! An independent ELF writer and allocation probe exercise the public scanner.
#![cfg(feature = "loader")]
use parrot_abi::{DylibLoader, LoadError, NativeArtifactLimits, Violation};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counted;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn allocated(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::SeqCst) + bytes;
    PEAK.fetch_max(live, Ordering::SeqCst);
}
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        System.dealloc(p, layout);
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = System.realloc(p, layout, size);
        if !result.is_null() {
            if size >= layout.size() {
                allocated(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::SeqCst);
            }
        }
        result
    }
}
#[global_allocator]
static ALLOCATOR: Counted = Counted;
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Artifact(std::path::PathBuf);
impl Drop for Artifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
// ELF64 little-endian, four named sections. Duplicate symbol references share
// one string-table entry. No linker/compiler or object writer is the oracle.
fn elf(names: &[&str]) -> Artifact {
    fn u16(b: &mut Vec<u8>, x: u16) {
        b.extend(x.to_le_bytes());
    }
    fn u32(b: &mut Vec<u8>, x: u32) {
        b.extend(x.to_le_bytes());
    }
    fn u64(b: &mut Vec<u8>, x: u64) {
        b.extend(x.to_le_bytes());
    }
    let mut strings = vec![0];
    let mut offsets = std::collections::BTreeMap::new();
    for &name in names {
        offsets.entry(name).or_insert_with(|| {
            let offset = strings.len() as u32;
            strings.extend(name.as_bytes());
            strings.push(0);
            offset
        });
    }
    let mut bytes = vec![0; 64];
    let tls = bytes.len() as u64;
    bytes.extend([0; 8]);
    let strtab = bytes.len() as u64;
    bytes.extend(&strings);
    let symtab = bytes.len() as u64;
    bytes.extend([0; 24]);
    for name in names {
        u32(&mut bytes, offsets[name]);
        bytes.extend([0x16, 0]);
        u16(&mut bytes, 1);
        u64(&mut bytes, 0);
        u64(&mut bytes, 8);
    }
    let section_names = b"\0.tdata\0.strtab\0.symtab\0.shstrtab\0";
    let shstr = bytes.len() as u64;
    bytes.extend(section_names);
    while !bytes.len().is_multiple_of(8) {
        bytes.push(0);
    }
    let section_offset = bytes.len() as u64;
    bytes.extend([0; 64]);
    for (name, kind, flags, offset, size, link, info, align, entry) in [
        (1, 1, 0x403, tls, 8, 0, 0, 8, 0),
        (8, 3, 0, strtab, strings.len() as u64, 0, 0, 1, 0),
        (16, 2, 0, symtab, 24 * (names.len() as u64 + 1), 2, 1, 8, 24),
        (24, 3, 0, shstr, section_names.len() as u64, 0, 0, 1, 0),
    ] {
        u32(&mut bytes, name);
        u32(&mut bytes, kind);
        u64(&mut bytes, flags);
        u64(&mut bytes, 0);
        u64(&mut bytes, offset);
        u64(&mut bytes, size);
        u32(&mut bytes, link);
        u32(&mut bytes, info);
        u64(&mut bytes, align);
        u64(&mut bytes, entry);
    }
    let mut header = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0".to_vec();
    u16(&mut header, 3);
    u16(&mut header, 183);
    u32(&mut header, 1);
    u64(&mut header, 0);
    u64(&mut header, 0);
    u64(&mut header, section_offset);
    u32(&mut header, 0);
    for n in [64, 0, 0, 64, 5, 4] {
        u16(&mut header, n);
    }
    bytes[..64].copy_from_slice(&header);
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "parrot-scan-{}-{}.elf",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).unwrap();
    Artifact(path)
}

#[test]
fn small_shared_name_artifact_cannot_amplify_heap_or_unbounded_scan_work() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let name = format!("component_owned_tls_{}", "x".repeat(32768));
    let artifact = elf(&vec![name.as_str(); 1024]);
    assert!(std::fs::metadata(&artifact.0).unwrap().len() < 60_000);
    for limits in [
        NativeArtifactLimits::default(),
        NativeArtifactLimits {
            max_scan_name_bytes: name.len() * 1024,
            ..Default::default()
        },
    ] {
        let baseline = LIVE.load(Ordering::SeqCst);
        PEAK.store(baseline, Ordering::SeqCst);
        let result = DylibLoader::scan_violations_with_limits(&artifact.0, limits);
        let peak = PEAK.load(Ordering::SeqCst).saturating_sub(baseline);
        assert!(
            peak < 4 * 1024 * 1024,
            "scan used {peak} bytes for a <60KB artifact"
        );
        if limits.max_scan_name_bytes == NativeArtifactLimits::default().max_scan_name_bytes {
            assert!(matches!(result, Err(LoadError::ResourceLimit(_))));
        } else {
            assert_eq!(
                result.unwrap(),
                vec![Violation::TlsDestructor {
                    symbol: name.clone()
                }]
            );
        }
    }
}

#[test]
fn exact_scan_budgets_succeed_and_each_exhausted_budget_fails_closed() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let artifact = elf(&["first", "second", "third"]);
    let limits = NativeArtifactLimits {
        max_scan_symbols: 3,
        max_scan_name_bytes: 16,
        max_scan_sections: 4,
        ..Default::default()
    };
    assert_eq!(
        DylibLoader::scan_violations_with_limits(&artifact.0, limits)
            .unwrap()
            .len(),
        3
    );
    for insufficient in [
        NativeArtifactLimits {
            max_scan_symbols: 2,
            ..limits
        },
        NativeArtifactLimits {
            max_scan_name_bytes: 15,
            ..limits
        },
        NativeArtifactLimits {
            max_scan_sections: 3,
            ..limits
        },
    ] {
        assert!(matches!(
            DylibLoader::scan_violations_with_limits(&artifact.0, insufficient),
            Err(LoadError::ResourceLimit(_))
        ));
        // An analysis quota error must stop loading before native initialization.
        assert!(matches!(
            unsafe { DylibLoader::load_with_limits(&artifact.0, "", insufficient) },
            Err(LoadError::ResourceLimit(_))
        ));
    }
}

#[test]
fn std_tls_prefix_exemption_preserves_owned_and_unknown_tls_rejection() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let artifact = elf(&[
        "_ZN3std3fooE",
        "_RNvC3std3foo",
        "_ZN3std13foo$LT$u8$GT$E",
        "_ZN3std13foo$LT$u8$GT$E$tlv$init",
        "component_tls",
    ]);
    assert_eq!(
        DylibLoader::scan_violations(&artifact.0).unwrap(),
        vec![Violation::TlsDestructor {
            symbol: "component_tls".into()
        }]
    );
    let stripped = elf(&[]);
    assert!(matches!(
        DylibLoader::scan_violations(&stripped.0),
        Err(LoadError::Scan(_))
    ));
}

#[test]
fn invalid_analysis_limits_are_configuration_errors_before_io() {
    let _guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    for bad in [0, usize::MAX] {
        for limits in [
            NativeArtifactLimits {
                max_scan_symbols: bad,
                ..Default::default()
            },
            NativeArtifactLimits {
                max_scan_name_bytes: bad,
                ..Default::default()
            },
            NativeArtifactLimits {
                max_scan_sections: bad,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                DylibLoader::scan_violations_with_limits(
                    std::path::Path::new("/no/such/image"),
                    limits
                ),
                Err(LoadError::InvalidConfig(_))
            ));
        }
    }
}
