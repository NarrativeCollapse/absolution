use std::{
    process::{Command, Stdio},
    sync::OnceLock,
};

#[derive(Debug, Default)]
pub struct IdentifierConfig {
    pub commit: String,
    pub time: String,
}

impl IdentifierConfig {
    fn new() -> Self {
        // Empty when the cookbook is not run from a git checkout.
        let commit = crate::cook::git::head(std::path::Path::new("."))
            .map(|head| head.rev)
            .unwrap_or_default();
        // better than importing heavy deps like chrono
        let time = String::from_utf8_lossy(
            Command::new("date")
                .arg("-u")
                .arg("+%Y-%m-%dT%H:%M:%SZ")
                .stdout(Stdio::piped())
                .output()
                .expect("Failed to get current ISO-formatted time")
                .stdout
                .trim_ascii(),
        )
        .into();
        IdentifierConfig { commit, time }
    }
}

static IDENTIFIER_CONFIG: OnceLock<IdentifierConfig> = OnceLock::new();

pub fn get_ident() -> &'static IdentifierConfig {
    IDENTIFIER_CONFIG
        .get()
        .expect("Identifier is not initialized")
}

pub fn init_ident() {
    IDENTIFIER_CONFIG
        .set(IdentifierConfig::new())
        .expect("Identifier is initialized twice")
}
