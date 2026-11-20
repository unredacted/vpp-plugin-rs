use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;

const BUILD_VER_PROBE: &str = "vpp_build_ver_probe.c";
const SHARED_HEAP_PROBE: &str = "shared_heap_probe.c";
const CHAIN_LAST_PROBE: &str = "chain_last_probe.c";
const MARCH_VARIANTS: &str = "march_variants.rs";

/// Every march variant `#[vlib_node]` registers a node function for, across
/// VPP versions. Must name every variant in vpp-plugin-macros'
/// `CPU_MARCH_TO_CPU_AND_TARGET_FEATURE` (other than `default`): the macro
/// refers to `macro_support::march_variant::<name>` for each, and a name
/// missing here fails the plugin's build.
///
/// VPP's variant list is per release (26.06 replaced x86's hsw, skx and icl
/// with x86_64_v3 and x86_64_v4), so each is `Some` only when the VPP being
/// built against defines it, and the macro registers only those.
const KNOWN_MARCH_VARIANTS: &[&str] = &[
    "scalar",
    "hsw",
    "skx",
    "icl",
    "x86_64_v3",
    "x86_64_v4",
    "octeontx2",
    "thunderx2t99",
    "cortexa72",
    "neoversen1",
    "neoversen2",
];

fn out_dir() -> PathBuf {
    PathBuf::from(env::var("OUT_DIR").unwrap())
}

fn headers_available() -> bool {
    let mut cc = cc::Build::new();
    cc.file("vlib_wrapper.h");
    cc.warnings_into_errors(true);
    if let Err(e) = cc.try_compile("libvlib_wrapper.a") {
        println!(
            "cargo:warning=Compile failed: {}, using pre-generated bindings",
            e
        );
        false
    } else {
        true
    }
}

fn build_wrapper() {
    println!("cargo:rerun-if-changed=vlib_wrapper.c");

    let mut cc = cc::Build::new();
    cc.file("vlib_wrapper.c");
    cc.warnings_into_errors(true);
    cc.compile("libvlib_wrapper.a");
}

/// `VPP_BUILD_VER` from the VPP headers being compiled against (`vpp/app/version.h`), read
/// through the C preprocessor so the compiler's own include path decides which VPP it is.
fn vpp_build_ver() -> String {
    let probe = out_dir().join(BUILD_VER_PROBE);
    std::fs::write(
        &probe,
        "#include <vpp/app/version.h>\nvpp_plugin_build_ver VPP_BUILD_VER\n",
    )
    .expect("write VPP_BUILD_VER probe");
    let expanded = cc::Build::new()
        .file(&probe)
        .cargo_metadata(false)
        .try_expand()
        .expect("preprocess vpp/app/version.h");
    build_ver_from_expansion(&String::from_utf8_lossy(&expanded))
        .expect("VPP_BUILD_VER not defined by vpp/app/version.h")
}

/// The string literal the probe's `vpp_plugin_build_ver VPP_BUILD_VER` line
/// expanded to. The marker and the string need not share a line: GCC puts a
/// line marker between them when the macro comes from a system header, as
/// it does for a VPP installed under /usr/include:
///
/// ```text
/// vpp_plugin_build_ver
/// # 2 "vpp_build_ver_probe.c" 3 4
///                     "26.06-release-octeon9"
/// ```
///
/// so take the first string literal after the marker, skipping line
/// markers. `None` when there is none (the macro is not defined).
fn build_ver_from_expansion(expanded: &str) -> Option<String> {
    let (_, after) = expanded.split_once("vpp_plugin_build_ver")?;
    let rest: Vec<&str> = after
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    let rest = rest.join(" ");
    let start = rest.find('"')? + 1;
    let len = rest[start..].find('"')?;
    Some(rest[start..start + len].to_owned())
}

/// VPP 26.06 moved feature-arc config strings into one heap shared by every
/// arc (`feature_main.shared_feature_config_heap`) and dropped
/// `vnet_buffer_opaque_t::feature_arc_index`; `vnet_feature_next` now walks
/// that heap by `current_config_index` alone. Probe for the field rather
/// than parse a version string so any build of either shape is handled.
fn has_shared_feature_config_heap() -> bool {
    let probe = out_dir().join(SHARED_HEAP_PROBE);
    std::fs::write(
        &probe,
        "#include <vnet/vnet.h>\n#include <vnet/feature/feature.h>\n\
         u32 *pf_probe (void) { return feature_main.shared_feature_config_heap; }\n",
    )
    .expect("write shared-heap probe");
    cc::Build::new()
        .file(&probe)
        .cargo_warnings(false)
        .cargo_metadata(false)
        .try_compile("shared_heap_probe")
        .is_ok()
}

/// VPP 26.10 grew `vlib_buffer_length_in_chain_slow_path` from `(vm, b_first)`
/// to `(vm, b_first, b_last, count)`, the two new out-parameters optional
/// (NULL, as `vlib_buffer_length_in_chain` passes them). Probe for the call
/// shape, as for the shared heap, rather than parse a version string.
fn has_chain_slow_path_last_count() -> bool {
    let probe = out_dir().join(CHAIN_LAST_PROBE);
    std::fs::write(
        &probe,
        "#include <vlib/vlib.h>\n#include <vlib/buffer_funcs.h>\n\
         uword pf_probe (vlib_main_t *vm, vlib_buffer_t *b)\n\
         { return vlib_buffer_length_in_chain_slow_path (vm, b, 0, 0); }\n",
    )
    .expect("write chain slow-path probe");
    cc::Build::new()
        .file(&probe)
        .cargo_warnings(false)
        .cargo_metadata(false)
        .try_compile("chain_last_probe")
        .is_ok()
}

/// Make Cargo rerun this script when a header that `sources` include
/// changes, so the bindings, `VPP_BUILD_VER` and the cfgs never outlive the
/// VPP they came from: a plugin built from stale struct layouts is
/// memory-unsafe, and one claiming the wrong `version_required` is refused.
/// The preprocessor lists the headers (`-M`), so its own include path
/// decides, as it does for everything else here.
///
/// Each header's directory is watched as well as the header. A package
/// manager installs a header with its archive's mtime, which can be older
/// than this script's last run (dpkg does, so upgrading the `vpp-dev` deb
/// alone would not register), but renaming it into place bumps the
/// directory's mtime, and Cargo takes the newest mtime under a watched
/// directory.
fn rerun_if_headers_change(sources: &[PathBuf]) {
    let out_dir = out_dir();
    let mut watched = BTreeSet::new();
    for source in sources {
        let rule = cc::Build::new()
            .file(source)
            .flag("-M")
            .cargo_metadata(false)
            .try_expand()
            .expect("list the headers the build compiles against");
        for dep in make_prerequisites(&String::from_utf8_lossy(&rule)) {
            let dep = PathBuf::from(dep);
            // The probes are rewritten every run: watching them would rerun every build.
            if dep.starts_with(&out_dir) {
                continue;
            }
            if let Some(dir) = dep.parent().filter(|d| !d.as_os_str().is_empty()) {
                watched.insert(dir.to_path_buf());
            }
            watched.insert(dep);
        }
    }
    // Cargo walks a watched directory whole, so anything under another is redundant.
    for path in &watched {
        if !path.ancestors().skip(1).any(|a| watched.contains(a)) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// The prerequisites of the make rule(s) `-M` printed: everything after a
/// target's `:`, across `\`-continued lines, with make's `\ ` and `$$`
/// escapes undone.
fn make_prerequisites(rules: &str) -> Vec<String> {
    let rules = rules.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut deps = Vec::new();
    for line in rules.lines() {
        let Some((_, prereqs)) = line.split_once(": ") else {
            continue;
        };
        let mut dep = String::new();
        let mut chars = prereqs.chars().peekable();
        while let Some(c) = chars.next() {
            match (c, chars.peek()) {
                ('\\', Some(' ')) | ('$', Some('$')) => dep.push(chars.next().unwrap()),
                (c, _) if c.is_whitespace() => {
                    if !dep.is_empty() {
                        deps.push(std::mem::take(&mut dep));
                    }
                }
                (c, _) => dep.push(c),
            }
        }
        if !dep.is_empty() {
            deps.push(dep);
        }
    }
    deps
}

/// Write `march_variants.rs`: a `const <name>: Option<clib_march_variant_type_t>`
/// per [`KNOWN_MARCH_VARIANTS`] entry, `Some` where `bindings` (the bindings'
/// source text) defines `CLIB_MARCH_VARIANT_TYPE_<name>`.
fn write_march_variants(bindings: &str) {
    let mut out =
        String::from("// @generated by vpp-plugin's build.rs from the VPP being built against.\n");
    for name in KNOWN_MARCH_VARIANTS {
        let constant = format!("clib_march_variant_type_t_CLIB_MARCH_VARIANT_TYPE_{name}");
        let value = if bindings.contains(&format!("pub const {constant}:")) {
            format!("Some(crate::bindings::{constant})")
        } else {
            "None".to_owned()
        };
        out.push_str(&format!(
            "pub const {name}: Option<crate::bindings::clib_march_variant_type_t> = {value};\n"
        ));
    }
    std::fs::write(out_dir().join(MARCH_VARIANTS), out).expect("write march_variants.rs");
}

fn main() {
    println!("cargo:rerun-if-changed=vlib_wrapper.h");
    println!("cargo:rustc-check-cfg=cfg(pregenerated_bindings)");
    println!("cargo:rustc-check-cfg=cfg(vpp_shared_feature_config_heap)");
    println!("cargo:rustc-check-cfg=cfg(vpp_chain_slow_path_last_count)");
    // Where the compilers look for VPP's headers besides their flags, which
    // cc (CC, CFLAGS, ...) and bindgen (BINDGEN_EXTRA_CLANG_ARGS, through
    // CargoCallbacks) declare themselves; and the libclang bindgen loads.
    for var in ["CPATH", "C_INCLUDE_PATH", "LIBCLANG_PATH"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    if !headers_available() {
        println!("cargo:rustc-cfg=pregenerated_bindings");
        // No VPP to pin to; an empty `version_required` is no requirement to VPP's loader.
        println!("cargo:rustc-env=VPP_PLUGIN_VPP_BUILD_VER=");
        // The pregenerated bindings, where src/bindings.rs includes them.
        let target = env::var("TARGET").unwrap_or_default();
        let pregenerated = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
            .join(format!("src/bindings/{target}.rs"));
        println!("cargo:rerun-if-changed={}", pregenerated.display());
        write_march_variants(&std::fs::read_to_string(&pregenerated).unwrap_or_default());
        return;
    }

    println!(
        "cargo:rustc-env=VPP_PLUGIN_VPP_BUILD_VER={}",
        vpp_build_ver()
    );

    if has_shared_feature_config_heap() {
        println!("cargo:rustc-cfg=vpp_shared_feature_config_heap");
    }

    if has_chain_slow_path_last_count() {
        println!("cargo:rustc-cfg=vpp_chain_slow_path_last_count");
    }

    build_wrapper();

    rerun_if_headers_change(&[
        PathBuf::from("vlib_wrapper.c"),
        out_dir().join(BUILD_VER_PROBE),
        out_dir().join(SHARED_HEAP_PROBE),
        out_dir().join(CHAIN_LAST_PROBE),
    ]);

    let bindings = bindgen::Builder::default()
        .header("vlib_wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_file("vlib_wrapper\\.h")
        .allowlist_file(".*/vlib/buffer\\.h")
        .allowlist_file(".*/vlib/buffer_funcs\\.h")
        .allowlist_file(".*/vlib/cli\\.h")
        .allowlist_file(".*/vlib/config\\.h")
        .allowlist_file(".*/vlib/counter\\.h")
        .allowlist_file(".*/vlib/defs\\.h")
        .allowlist_file(".*/vlib/global_funcs\\.h")
        .allowlist_file(".*/vlib/init\\.h")
        .allowlist_file(".*/vlib/main\\.h")
        .allowlist_file(".*/vlib/node\\.h")
        .allowlist_file(".*/vlib/threads\\.h")
        .allowlist_file(".*/vlib/trace_funcs\\.h")
        .allowlist_file(".*/vlib/unix/plugin\\.h")
        .allowlist_file(".*/vlibapi/api\\.h")
        .allowlist_file(".*/vlibapi/api_common\\.h")
        .allowlist_file(".*/vlibapi/memory_shared\\.h")
        .allowlist_file(".*/vlibmemory/api\\.h")
        .allowlist_file(".*/vnet/buffer\\.h")
        .allowlist_file(".*/vnet/error\\.h")
        .allowlist_file(".*/vnet/feature/feature\\.h")
        .allowlist_file(".*/vnet/global_funcs\\.h")
        .allowlist_file(".*/vppinfra/config\\.h")
        .allowlist_file(".*/vppinfra/error\\.h")
        .allowlist_file(".*/vppinfra/error_bootstrap\\.h")
        .allowlist_file(".*/vppinfra/format\\.h")
        .allowlist_file(".*/vppinfra/mem\\.h")
        .allowlist_file(".*/vppinfra/vec\\.h")
        .allowlist_file(".*/vppinfra/vec_bootstrap\\.h")
        // allowing .*/vnet/feature/feature\\.h causes errors in generated code due to duplicate definitions
        .allowlist_item("feature_main")
        .allowlist_type("ip4_header_t")
        .allowlist_type("ip6_header_t")
        .allowlist_type("format_ip4_header")
        .allowlist_type("format_ip6_header")
        .allowlist_type("format_vnet_sw_if_index_name")
        // bindgen generates duplicate definitions for these types due to forward declarations in vlib/trace.h
        .blocklist_type("vlib_trace_main_t")
        .blocklist_type("vlib_buffer_t")
        // bindgen generates duplicate definitions for these types due to forward declarations in vnet/interface.h, so we blocklist them and declare them manually here
        .blocklist_type("vnet_sw_interface_t")
        // Avoid bindgen-generated code causing this to have an alignment of 8 instead of 1
        .blocklist_type("ip6_address_t")
        .flexarray_dst(true)
        .derive_default(true)
        .layout_tests(false)
        // Can cause include path to not match compiler when multiple versions of clang installed
        .detect_include_paths(false)
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("vlib_bindings.rs"))
        .expect("Couldn't write vlib_bindings!");
    write_march_variants(&bindings.to_string());

    println!("cargo:rustc-link-lib=vlibmemory");
    println!("cargo:rustc-link-lib=vnet");
    println!("cargo:rustc-link-lib=vlibapi");
    println!("cargo:rustc-link-lib=vlib");
    println!("cargo:rustc-link-lib=vppinfra");
}
