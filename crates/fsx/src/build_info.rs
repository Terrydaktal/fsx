//! Compile-time build identity shared by every fsx executable.
//!
//! The values are embedded during compilation. No filesystem, environment,
//! clock, or subprocess access is performed by the runtime API.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildInfo {
    pub package: &'static str,
    pub version: &'static str,
    pub git_sha: &'static str,
    pub git_dirty: bool,
    pub target: &'static str,
    pub profile: &'static str,
    pub rustc: &'static str,
}

pub fn current(package: &'static str, version: &'static str) -> BuildInfo {
    BuildInfo {
        package,
        version,
        git_sha: match option_env!("FSX_GIT_SHA") {
            Some(value) => value,
            None => "unknown",
        },
        git_dirty: matches!(option_env!("FSX_GIT_DIRTY"), Some("true")),
        target: env!("FSX_TARGET"),
        profile: env!("FSX_PROFILE"),
        rustc: match option_env!("RUSTC_VERSION") {
            Some(value) => value,
            None => "unknown",
        },
    }
}

pub fn print_json(info: BuildInfo) {
    println!(
        "{{\"package\":\"{}\",\"version\":\"{}\",\"git_sha\":\"{}\",\"git_dirty\":{},\"target\":\"{}\",\"profile\":\"{}\",\"rustc\":\"{}\"}}",
        escape(info.package),
        escape(info.version),
        escape(info.git_sha),
        info.git_dirty,
        escape(info.target),
        escape(info.profile),
        escape(info.rustc),
    );
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes_identity_fields() {
        let info = BuildInfo {
            package: "tool",
            version: "1",
            git_sha: "sha\"x",
            git_dirty: false,
            target: "target",
            profile: "release",
            rustc: "rustc",
        };
        assert!(escape(info.git_sha).contains("\\\""));
    }
}
