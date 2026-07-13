use serde_json::json;
use young_tool_runtime::{
    CapabilityManifest, McpCompatibility, ToolApprovalPolicy, ToolSafetyClass,
};

#[test]
fn built_in_manifest_loads_capability_and_tool_metadata_from_toml() {
    let source = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"

[tools.input_schema]
type = "object"
required = ["path"]

[tools.input_schema.properties.path]
type = "string"

[tools.mcp]
server = "builtin-coding"
tool_name = "read_file"
protocol_version = "reserved"
"#;

    let manifest = CapabilityManifest::from_toml(source).expect("manifest loads");

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.capability.id, "coding");
    assert_eq!(manifest.capability.version, "0.1.0");
    assert_eq!(manifest.capability.name, "Coding");
    assert_eq!(manifest.capability.description, "Built-in coding tools.");
    assert_eq!(manifest.tools.len(), 1);
    assert_eq!(manifest.tools[0].safety_class, ToolSafetyClass::AlwaysAllow);
    assert_eq!(manifest.tools[0].input_schema["type"], json!("object"));

    let definitions = manifest
        .tool_definitions()
        .expect("loaded manifest produces valid definitions");
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "read_file");
    assert_eq!(definitions[0].capability.id, "coding");
    assert_eq!(definitions[0].capability.version, "0.1.0");
    assert_eq!(
        definitions[0].approval_policy,
        ToolApprovalPolicy::AlwaysAllow
    );
    assert_eq!(
        definitions[0].mcp,
        Some(McpCompatibility {
            server: "builtin-coding".to_string(),
            tool_name: "read_file".to_string(),
            protocol_version: "reserved".to_string(),
        })
    );
}

#[test]
fn built_in_manifest_rejects_duplicate_tool_names_with_a_useful_error() {
    let source = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = { type = "object" }

[[tools]]
name = "read_file"
description = "A conflicting declaration."
safety_class = "always_allow"
input_schema = { type = "object" }
"#;

    let error = CapabilityManifest::from_toml(source).expect_err("duplicates are invalid");

    assert!(
        error
            .to_string()
            .contains("duplicate tool name 'read_file'"),
        "unexpected error: {error}"
    );
}

#[test]
fn built_in_manifest_requires_a_reason_for_non_allowing_safety_classes() {
    let source = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "apply_patch"
description = "Apply a patch inside the workspace."
safety_class = "requires_approval"
input_schema = { type = "object" }
"#;

    let error = CapabilityManifest::from_toml(source).expect_err("reason is required");

    assert!(
        error
            .to_string()
            .contains("tool 'apply_patch': approval policy reason must not be empty"),
        "unexpected error: {error}"
    );
}

#[test]
fn built_in_manifest_reports_the_missing_required_field() {
    let source = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
safety_class = "always_allow"
input_schema = { type = "object" }
"#;

    let error = CapabilityManifest::from_toml(source).expect_err("description is required");
    let message = error.to_string();

    assert!(message.contains("failed to parse built-in capability manifest TOML"));
    assert!(message.contains("description"), "unexpected error: {error}");
}

#[test]
fn built_in_manifest_rejects_unknown_schema_versions() {
    let source = r#"
schema_version = 2

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = { type = "object" }
"#;

    let error = CapabilityManifest::from_toml(source).expect_err("version is unsupported");

    assert!(
        error
            .to_string()
            .contains("unsupported schema_version 2; expected 1"),
        "unexpected error: {error}"
    );
}

#[test]
fn built_in_manifest_rejects_empty_metadata_and_non_object_input_schemas() {
    let empty_capability_id = r#"
schema_version = 1

[capability]
id = "  "
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = { type = "object" }
"#;
    let non_object_schema = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = "not-an-object"
"#;

    let cases = [
        (empty_capability_id, "capability.id must not be empty"),
        (
            non_object_schema,
            "tool 'read_file': input_schema must be an object",
        ),
    ];

    for (source, expected) in cases {
        let error = CapabilityManifest::from_toml(source).expect_err("manifest is invalid");
        assert!(
            error.to_string().contains(expected),
            "expected '{expected}', got '{error}'"
        );
    }
}

#[test]
fn direct_deserialization_still_cannot_produce_invalid_tool_definitions() {
    let source = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = "not-an-object"
"#;
    let manifest: CapabilityManifest =
        toml::from_str(source).expect("serde shape is syntactically valid");

    let error = manifest
        .tool_definitions()
        .expect_err("conversion owns definition validation");

    assert!(
        error
            .to_string()
            .contains("tool 'read_file': input_schema must be an object"),
        "unexpected error: {error}"
    );
}

#[test]
fn direct_deserialization_cannot_bypass_manifest_invariants_during_conversion() {
    let unsupported_schema_version = r#"
schema_version = 2

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = { type = "object" }
"#;
    let duplicate_tool_names = r#"
schema_version = 1

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."

[[tools]]
name = "read_file"
description = "Read one workspace file."
safety_class = "always_allow"
input_schema = { type = "object" }

[[tools]]
name = "read_file"
description = "Conflicting declaration."
safety_class = "always_allow"
input_schema = { type = "object" }
"#;
    let empty_tools = r#"
schema_version = 1
tools = []

[capability]
id = "coding"
version = "0.1.0"
name = "Coding"
description = "Built-in coding tools."
"#;

    let cases = [
        (unsupported_schema_version, "unsupported schema_version 2"),
        (duplicate_tool_names, "duplicate tool name 'read_file'"),
        (empty_tools, "tools must contain at least one tool"),
    ];

    for (source, expected) in cases {
        let manifest: CapabilityManifest =
            toml::from_str(source).expect("serde shape is syntactically valid");
        let borrowed_error = manifest
            .tool_definitions()
            .expect_err("borrowed conversion owns all manifest invariants");
        let consumed_error = manifest
            .into_tool_definitions()
            .expect_err("consuming conversion owns all manifest invariants");

        for error in [borrowed_error, consumed_error] {
            assert!(
                error.to_string().contains(expected),
                "expected '{expected}', got '{error}'"
            );
        }
    }
}
