//! Regression guard for the 2026-08-26 always-dirty-rebuild incident.
//!
//! `crate-type = ["cdylib", ...]` on this crate is a workspace-wide build-performance
//! bug, not a local choice. For a lib target whose crate-type list contains `cdylib`,
//! cargo emits the artifact WITHOUT `-C extra-filename=<hash>`, because a shared object
//! needs a stable SONAME. sigil-shield then becomes the only crate in the tree writing
//! to an unhashed `target/<profile>/deps/libsigil_shield.rlib`, so every one of its
//! units (each feature set, each profile, each concurrent agent) collides on that single
//! path. Every build rewrites the file, its mtime jumps, and cargo correctly marks the
//! entire downstream graph stale — sigil-state -> sigil-{emission,events,oracle,
//! braidpool} -> sigil-{api,chronos} -> sigil-node.
//!
//! Measured on 2026-08-26 with `cdylib` declared: a NO-OP
//! `build -p sigil-node --profile release-fast` rebuilt ~15 crates in 63s, then took
//! 107s on an identical immediately-following run — it never converged. With `rlib`
//! only, the same no-op is 1.1s.
//!
//! The browser wallet still gets its `.wasm`: the cdylib crate-type is applied at BUILD
//! time via `cargo rustc --crate-type cdylib`, never declared in the manifest. See
//! `scripts/build-shield-wasm.sh`.

/// The manifest must declare exactly one crate-type: `rlib`.
#[test]
fn manifest_declares_rlib_only() {
    let manifest = include_str!("../Cargo.toml");

    let line = manifest
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("crate-type"))
        .expect("sigil-shield/Cargo.toml must declare an explicit [lib] crate-type");

    assert!(
        !line.contains("cdylib"),
        "sigil-shield/Cargo.toml declares cdylib ({line:?}).\n\
         This silently destroys incremental builds for the whole workspace: cargo drops \
         the filename hash for cdylib lib targets, so all sigil-shield units collide on \
         one unhashed libsigil_shield.rlib and every build invalidates sigil-node's \
         entire dependency graph (measured: 1.1s no-op -> 63-107s).\n\
         Build the wasm with `scripts/build-shield-wasm.sh` \
         (cargo rustc --crate-type cdylib) instead."
    );

    assert_eq!(
        line, r#"crate-type = ["rlib"]"#,
        "expected exactly `crate-type = [\"rlib\"]`, found {line:?}"
    );
}
