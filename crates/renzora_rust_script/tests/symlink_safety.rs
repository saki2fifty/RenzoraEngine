//! Symlink safety tests for the discovery walker (correction 12).
//!
//! Phase 1 commits to "do not recurse through directory symlinks":
//! any directory entry that is itself a symlink (regardless of target)
//! is skipped during the initial walk, the watcher reconcile, and the
//! exporter walk. The same walker is reused across all three.

#[cfg(unix)]
mod unix_only {
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Build a project tree with one nested script, plus a symlink
    /// pointing to an external directory. After discovery, the script
    /// inside the symlink target MUST NOT appear in the canonical-id
    /// list (the symlink is skipped).
    #[test]
    fn discovery_skips_directory_symlinks() {
        // Project tree.
        let project = tempfile::tempdir().unwrap();
        let inside = project.path().join("inside");
        fs::create_dir_all(&inside).unwrap();
        fs::write(
            inside.join("script.rs"),
            "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n",
        )
        .unwrap();

        // External directory with its own script.
        let external = tempfile::tempdir().unwrap();
        fs::write(
            external.path().join("external.rs"),
            "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n",
        )
        .unwrap();

        // Symlink from project root to external.
        let link = project.path().join("link_to_external");
        symlink(external.path(), &link).unwrap();

        let ids = renzora_rust_script::discovery::collect_canonical_scripts(project.path());
        let paths: Vec<String> = ids.iter().map(|c| c.path().to_string()).collect();

        // The inside script is reachable directly.
        assert!(
            paths.contains(&"inside/script.rs".to_string()),
            "direct path missing: {paths:?}"
        );
        // The external script must NOT be reachable through the symlink.
        assert!(
            !paths.iter().any(|p| p.contains("external.rs")),
            "symlink target must not appear in canonical ids: {paths:?}"
        );
    }

    /// A symlink loop must not recurse forever. The walker bails out at
    /// the symlink itself (it does not follow), so a self-loop or
    /// nested loop never enters the recursion.
    #[test]
    fn discovery_does_not_enter_symlink_loop() {
        let project = tempfile::tempdir().unwrap();
        // Create a loop: project/loop -> project.
        let loop_link = project.path().join("loop");
        symlink(project.path(), &loop_link).unwrap();
        // Add a real script for the walker to find.
        fs::write(
            project.path().join("script.rs"),
            "fn update(_: &mut renzora::ScriptCtx) {}\nrenzora::script!(update);\n",
        )
        .unwrap();
        // The walker does not follow loops, so the symlink is skipped
        // and the walk terminates promptly.
        let ids = renzora_rust_script::discovery::collect_canonical_scripts(project.path());
        let paths: Vec<String> = ids.iter().map(|c| c.path().to_string()).collect();
        assert!(paths.contains(&"script.rs".to_string()));
    }
}
