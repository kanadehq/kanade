//! Guards the opt-in role-level NATS users deployment path against being
//! dropped by an unrelated change. These are plain string checks on the deploy
//! entry points; behaviour is covered by the deploy scripts' own tests and the
//! real-broker suites.

const SETUP_SH: &str = include_str!("../../../deploy/linux/setup.sh");
const NATS_PS1: &str = include_str!("../../../scripts/deploy/nats.ps1");
const INTEGRATION_YML: &str = include_str!("../../../.github/workflows/integration.yml");

fn assert_has(haystack: &str, needle: &str, what: &str) {
    assert!(
        haystack.contains(needle),
        "{what} no longer contains `{needle}`: the opt-in role-level NATS users deployment path was removed"
    );
}

#[test]
fn linux_setup_keeps_auth_mode_switch() {
    assert_has(SETUP_SH, "KANADE_NATS_AUTH_MODE", "deploy/linux/setup.sh");
    assert_has(SETUP_SH, "nats-auth-mode", "deploy/linux/setup.sh");
}

#[test]
fn windows_deploy_keeps_user_switches() {
    assert_has(NATS_PS1, "UseNatsUsers", "scripts/deploy/nats.ps1");
    assert_has(NATS_PS1, "UseNatsToken", "scripts/deploy/nats.ps1");
}

#[test]
fn ci_keeps_running_auth_mode_deploy_tests() {
    assert_has(
        INTEGRATION_YML,
        "deploy/test-nats-auth-mode.sh",
        "integration.yml",
    );
    assert_has(INTEGRATION_YML, "nats-users.Tests.ps1", "integration.yml");
}
