use std::io::IsTerminal;
use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password};
use wi_source::credentials::{Credentials, GoogleCredentials, JiraCredentials, SlackCredentials};

use super::{GlobalOpts, find_config, load_config};

/// Interactive wizard that writes `<vault>/.wiki-ingest/credentials.json`. Seeds defaults
/// from any existing file so re-running edits in place; secrets can be kept by pressing
/// enter. Refuses to run without a terminal (e.g. piped/CI) since it can't prompt.
pub async fn credentials(opts: &GlobalOpts, vault_override: Option<PathBuf>) -> miette::Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(miette::miette!(
            "`wi init credentials` is interactive and needs a terminal.\n\
             Non-interactively, copy credentials.example.json to \
             <vault>/.wiki-ingest/credentials.json and edit it, or set the WI_* env vars."
        ));
    }

    let vault_root = match vault_override {
        Some(v) => v,
        None => load_config(&find_config(opts)?)?.vault.root_path(),
    };
    // Seed defaults from the existing file, but never let a malformed file block the
    // wizard — fixing credentials is exactly its job.
    let mut creds = Credentials::from_file(&vault_root).unwrap_or_else(|e| {
        eprintln!("! existing credentials.json couldn't be parsed ({e}); starting fresh.");
        Credentials::default()
    });

    eprintln!(
        "Configuring credentials for vault: {}",
        vault_root.display()
    );
    eprintln!("Press enter to keep an existing secret. Decline a section to leave it as-is.\n");

    if confirm(
        "Configure Google (Gmail / Drive / Calendar)?",
        creds.google.is_some(),
    )? {
        let existing = creds.google.as_ref();
        creds.google = Some(GoogleCredentials {
            client_id: input("  client_id", existing.map(|g| g.client_id.as_str()))?,
            client_secret: secret(
                "  client_secret",
                existing.map(|g| g.client_secret.as_str()),
            )?,
            refresh_token: secret(
                "  refresh_token",
                existing.map(|g| g.refresh_token.as_str()),
            )?,
        });
    }

    if confirm("Configure Slack?", creds.slack.is_some())? {
        let existing = creds.slack.clone().unwrap_or_default();
        // Both optional; slack-search needs a user token, the channel reader takes either.
        let bot = optional_secret(
            "  bot_token (xoxb-…, enter to skip)",
            existing.bot_token.as_deref(),
        )?;
        let user = optional_secret(
            "  user_token (xoxp-…, enter to skip)",
            existing.user_token.as_deref(),
        )?;
        if bot.is_none() && user.is_none() {
            eprintln!("  (no token entered — leaving Slack unconfigured)");
            creds.slack = None;
        } else {
            creds.slack = Some(SlackCredentials {
                bot_token: bot,
                user_token: user,
            });
        }
    }

    if confirm("Configure Jira?", creds.jira.is_some())? {
        let existing = creds.jira.as_ref();
        creds.jira = Some(JiraCredentials {
            base_url: input(
                "  base_url (https://org.atlassian.net)",
                existing.map(|j| j.base_url.as_str()),
            )?,
            email: input("  email", existing.map(|j| j.email.as_str()))?,
            api_token: secret("  api_token", existing.map(|j| j.api_token.as_str()))?,
        });
    }

    let path = creds
        .save(&vault_root)
        .map_err(|e| miette::miette!("{e}"))?;
    eprintln!("\n✓ Wrote {} (owner-only, 0600)", path.display());
    eprintln!("  Run `wi validate` then `wi ingest`.");
    Ok(())
}

fn confirm(prompt: &str, default: bool) -> miette::Result<bool> {
    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(|e| miette::miette!("prompt: {e}"))
}

/// Visible text field, pre-filled with the existing value if present.
fn input(prompt: &str, existing: Option<&str>) -> miette::Result<String> {
    let mut builder = Input::<String>::new().with_prompt(prompt);
    if let Some(v) = existing {
        builder = builder.with_initial_text(v.to_string());
    }
    builder
        .interact_text()
        .map_err(|e| miette::miette!("prompt: {e}"))
}

/// Masked optional secret. Empty entry keeps the existing value (if any) or means "unset".
fn optional_secret(prompt: &str, existing: Option<&str>) -> miette::Result<Option<String>> {
    let entered = Password::new()
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()
        .map_err(|e| miette::miette!("prompt: {e}"))?;
    Ok(if entered.is_empty() {
        existing.map(str::to_string)
    } else {
        Some(entered)
    })
}

/// Masked secret field. When a value already exists, an empty entry keeps it (so the user
/// doesn't have to re-type tokens); otherwise a non-empty value is required.
fn secret(prompt: &str, existing: Option<&str>) -> miette::Result<String> {
    if let Some(current) = existing {
        let entered = Password::new()
            .with_prompt(format!("{prompt} (enter to keep)"))
            .allow_empty_password(true)
            .interact()
            .map_err(|e| miette::miette!("prompt: {e}"))?;
        Ok(if entered.is_empty() {
            current.to_string()
        } else {
            entered
        })
    } else {
        Password::new()
            .with_prompt(prompt)
            .interact()
            .map_err(|e| miette::miette!("prompt: {e}"))
    }
}
