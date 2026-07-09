//! Restart-authority evidence for durable supervise roots.

use anyhow::{bail, Result};

pub(crate) const LAUNCHD_LABEL_ENV: &str = "FKST_LAUNCHD_LABEL";
const XPC_SERVICE_NAME_ENV: &str = "XPC_SERVICE_NAME";

pub(crate) fn ensure_restart_authority() -> Result<RestartAuthorityEvidence> {
    let evidence = RestartAuthorityEvidence::from_env();
    if evidence.has_restart_authority() {
        return Ok(evidence);
    }

    bail!(
        "durable supervise requires launchd restart authority evidence: set non-empty `{}` from launchd and `{}` from the rendered launchd unit",
        XPC_SERVICE_NAME_ENV,
        LAUNCHD_LABEL_ENV
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RestartAuthorityEvidence {
    launchd_service_name: Option<String>,
    expected_launchd_label: Option<String>,
}

impl RestartAuthorityEvidence {
    fn from_env() -> Self {
        Self {
            launchd_service_name: cleaned_env(XPC_SERVICE_NAME_ENV),
            expected_launchd_label: cleaned_env(LAUNCHD_LABEL_ENV),
        }
    }

    pub(crate) fn has_restart_authority(&self) -> bool {
        let Some(service_name) = self.launchd_service_name.as_deref() else {
            return false;
        };
        let Some(expected_label) = self.expected_launchd_label.as_deref() else {
            return false;
        };
        service_name == expected_label
    }
}

fn cleaned_env(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let value = value.trim();
    if value.is_empty() || value == "0" {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::ENV_LOCK;

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old }
        }

        fn unset(key: &'static str) -> Self {
            let old = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = self.old.take() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn matching_launchd_service_name_satisfies_restart_authority() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _service = EnvGuard::set(XPC_SERVICE_NAME_ENV, "com.example.fkst");
        let _label = EnvGuard::set(LAUNCHD_LABEL_ENV, "com.example.fkst");

        assert!(ensure_restart_authority().is_ok());
    }

    #[test]
    fn missing_launchd_service_name_fails_closed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _service = EnvGuard::unset(XPC_SERVICE_NAME_ENV);
        let _label = EnvGuard::set(LAUNCHD_LABEL_ENV, "com.example.fkst");

        let err = ensure_restart_authority().unwrap_err();

        assert!(format!("{err:#}").contains("launchd restart authority evidence"));
    }

    #[test]
    fn shell_inherited_launchd_sentinel_fails_closed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _service = EnvGuard::set(XPC_SERVICE_NAME_ENV, "0");
        let _label = EnvGuard::set(LAUNCHD_LABEL_ENV, "com.example.fkst");

        assert!(ensure_restart_authority().is_err());
    }

    #[test]
    fn mismatched_launchd_label_fails_closed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _service = EnvGuard::set(XPC_SERVICE_NAME_ENV, "com.example.other");
        let _label = EnvGuard::set(LAUNCHD_LABEL_ENV, "com.example.fkst");

        assert!(ensure_restart_authority().is_err());
    }
}
