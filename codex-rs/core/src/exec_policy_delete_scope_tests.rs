use super::*;
use pretty_assertions::assert_eq;

/// A project directory below a temp dir, plus a scope for it.
fn project() -> (tempfile::TempDir, DeleteScope) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(cwd.join("sub")).expect("create project");
    let scope = DeleteScope::new(
        &cwd,
        /*user_home*/ None,
        &[],
        /*case_insensitive*/ cfg!(windows),
        /*check_filesystem*/ true,
    )
    .expect("scope");
    (dir, scope)
}

/// "Full Access": approvals disabled and no sandbox.
async fn full_access(
    command: &[&str],
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> ExecApprovalRequirement {
    let command: Vec<String> = command.iter().map(|part| (*part).to_string()).collect();
    ExecPolicyManager::default()
        .create_exec_approval_requirement_for_command_platform(
            ExecApprovalRequest {
                command: &command,
                approval_policy: AskForApproval::Never,
                permission_profile: PermissionProfile::Disabled,
                environment_policy: None,
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                sandbox_permissions: SandboxPermissions::UseDefault,
                prefix_rule: None,
                allow_prefix_rules: AllowPrefixRules::Honor,
                delete_scope: scope,
            },
            platform,
        )
        .await
}

fn is_skip(requirement: &ExecApprovalRequirement) -> bool {
    matches!(requirement, ExecApprovalRequirement::Skip { .. })
}

fn forbidden_reason(requirement: &ExecApprovalRequirement) -> Option<&str> {
    match requirement {
        ExecApprovalRequirement::Forbidden { reason } => Some(reason.as_str()),
        _ => None,
    }
}

#[tokio::test]
async fn windows_forced_delete_inside_the_scope_runs_in_full_access() {
    let (_dir, scope) = project();
    for script in [
        "Remove-Item -LiteralPath 'artifacts/test.bin' -Force",
        "Remove-Item -Path sub/out -Recurse -Force",
        "ri -Force test.bin",
    ] {
        let requirement = full_access(
            &["powershell.exe", "-Command", script],
            DangerousCommandPlatform::Windows,
            Some(&scope),
        )
        .await;
        assert!(is_skip(&requirement), "{script}: {requirement:?}");
    }
}

#[tokio::test]
async fn windows_forced_delete_without_a_scope_is_still_rejected_with_an_explanation() {
    let requirement = full_access(
        &[
            "powershell.exe",
            "-Command",
            "Remove-Item -LiteralPath 'artifacts/test.bin' -Force",
        ],
        DangerousCommandPlatform::Windows,
        /*scope*/ None,
    )
    .await;
    let reason = forbidden_reason(&requirement).expect("rejected");
    assert!(reason.contains("forced delete"), "{reason}");
    assert!(reason.contains("literal paths"), "{reason}");
}

#[tokio::test]
async fn windows_forced_delete_outside_the_scope_is_rejected() {
    let (_dir, scope) = project();
    for script in [
        "Remove-Item -Recurse -Force .",
        "Remove-Item ../elsewhere.bin -Force",
        "Remove-Item * -Force",
        "Remove-Item $target -Force",
        "Remove-Item .git -Recurse -Force",
        "Get-ChildItem sub | Remove-Item -Force",
    ] {
        let requirement = full_access(
            &["powershell.exe", "-Command", script],
            DangerousCommandPlatform::Windows,
            Some(&scope),
        )
        .await;
        let reason = forbidden_reason(&requirement)
            .unwrap_or_else(|| panic!("{script} must be rejected: {requirement:?}"));
        assert!(reason.contains("forced delete"), "{script}: {reason}");
    }
}

#[tokio::test]
async fn posix_forced_rm_is_scoped_the_same_way() {
    let (_dir, scope) = project();
    for command in [vec!["rm", "-rf", "build"], vec!["rm", "-f", "sub/out.bin"]] {
        let requirement =
            full_access(&command, DangerousCommandPlatform::Posix, Some(&scope)).await;
        assert!(is_skip(&requirement), "{command:?}: {requirement:?}");
    }
    for command in [
        vec!["rm", "-rf", "/"],
        vec!["rm", "-rf", "../elsewhere"],
        vec!["rm", "-rf", "*"],
        vec!["sudo", "rm", "-rf", "build"],
    ] {
        let requirement =
            full_access(&command, DangerousCommandPlatform::Posix, Some(&scope)).await;
        let reason = forbidden_reason(&requirement)
            .unwrap_or_else(|| panic!("{command:?} must be rejected: {requirement:?}"));
        assert!(
            reason.ends_with("rm -f style commands are not permitted. Use a safer approach"),
            "{command:?}: {reason}"
        );
    }
}
