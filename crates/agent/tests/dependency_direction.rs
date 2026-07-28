//! Compile-time-ish constraint test: asserts that the lower-level `flash-core`
//! and `flash-tools` crates do NOT reverse-depend on `flash-agent`. This keeps
//! the dependency direction one-way (agent -> core/tools) and prevents circular
//! coupling from creeping in.

use serde_json::Value;
use std::process::Command;

#[test]
fn flash_core_and_tools_must_not_depend_on_flash_agent() {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to invoke `cargo metadata`");
    assert!(
        output.status.success(),
        "`cargo metadata` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata produced invalid JSON");

    let mut checked = Vec::new();
    for package in metadata["packages"]
        .as_array()
        .expect("metadata `packages` is not an array")
    {
        let name = package["name"].as_str().expect("package missing `name`");
        if name == "flash-core" || name == "flash-tools" {
            checked.push(name.to_string());
            for dependency in package["dependencies"]
                .as_array()
                .expect("package `dependencies` is not an array")
            {
                let dep_name = dependency["name"]
                    .as_str()
                    .expect("dependency missing `name`");
                assert_ne!(
                    dep_name, "flash-agent",
                    "{name} must not depend on flash-agent (dependency direction violated)"
                );
            }
        }
    }

    assert_eq!(
        checked,
        ["flash-core", "flash-tools"],
        "expected to check both flash-core and flash-tools; metadata may have changed"
    );
}
