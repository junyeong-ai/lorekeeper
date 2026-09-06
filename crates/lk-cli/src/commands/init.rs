use std::io::IsTerminal;
use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password};
use lk_source::build_google_refresh_token;
use lk_source::credentials::{
    AtlassianAuthMethod, AtlassianCredentials, Credentials, GoogleCredentials, SlackCredentials,
};

use super::{GlobalOptions, find_config, load_config};

/// Interactive wizard that writes `<vault>/.lorekeeper/credentials.json`. Seeds defaults
/// from any existing file so re-running edits in place; secrets can be kept by pressing
/// enter. Refuses to run without a terminal (e.g. piped/CI) since it can't prompt.
pub async fn credentials(
    opts: &GlobalOptions,
    vault_override: Option<PathBuf>,
) -> miette::Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(miette::miette!(
            "`lore init credentials` is interactive and needs a terminal.\n\
             Non-interactively, copy credentials.example.json to \
             <vault>/.lorekeeper/credentials.json and edit it, or set the LORE_* env vars."
        ));
    }

    let vault_root = match vault_override {
        Some(v) => v,
        None => load_config(&find_config(opts)?)?.vault.root_path(),
    };
    // Seed defaults from the existing file, but never let a malformed file block the
    // wizard — fixing credentials is exactly its job.
    let mut creds = Credentials::load_file(&vault_root).unwrap_or_else(|e| {
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
        let existing = creds.google.clone();
        let client_id = input(
            "  client_id",
            existing.as_ref().map(|g| g.client_id.as_str()),
        )?;
        let client_secret = secret(
            "  client_secret",
            existing.as_ref().map(|g| g.client_secret.as_str()),
        )?;
        let refresh_token =
            google_refresh_token(existing.as_ref(), &client_id, &client_secret).await?;
        creds.google = Some(GoogleCredentials {
            client_id,
            client_secret,
            refresh_token,
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

    if confirm(
        "Configure an Atlassian instance (Jira + Confluence)?",
        !creds.atlassian.is_empty(),
    )? {
        atlassian_instance(&mut creds).await?;
    }

    let path = creds
        .save(&vault_root)
        .map_err(|e| miette::miette!("{e}"))?;
    eprintln!("\n✓ Wrote {} (owner-only, 0600)", path.display());
    eprintln!("  Run `lore validate` then `lore ingest`.");
    Ok(())
}

/// Configure one named Atlassian instance, which both the Jira and Confluence adapters read.
///
/// The auth method is asked BEFORE anything else because it determines which fields even
/// exist: a Cloud OAuth grant needs an app and a browser round-trip, a Cloud API token needs
/// an account email, and a Data Center PAT needs neither.
async fn atlassian_instance(creds: &mut Credentials) -> miette::Result<()> {
    let default_name = if creds.atlassian.is_empty() {
        "default".to_string()
    } else {
        creds
            .atlassian
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "default".into())
    };
    if !creds.atlassian.is_empty() {
        eprintln!(
            "  (configured instances: {})",
            creds
                .atlassian
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let name = input("  instance name", Some(&default_name))?;
    let existing = creds.atlassian.get(&name);

    let methods = [
        "oauth — Cloud via the gateway; reaches an IP-allowlisted site from any address, needs an app",
        "api-token — Cloud classic token, sent to the site; simplest to set up",
        "scoped-token — Cloud scoped token, sent to the gateway; no app needed",
        "pat — Data Center / Server personal access token",
    ];
    let default_method = match existing.map(|e| &e.auth) {
        Some(AtlassianAuthMethod::ApiToken { .. }) => 1,
        Some(AtlassianAuthMethod::ScopedToken { .. }) => 2,
        Some(AtlassianAuthMethod::PersonalAccessToken { .. }) => 3,
        _ => 0,
    };
    let choice = dialoguer::Select::new()
        .with_prompt("  auth method")
        .items(methods)
        .default(default_method)
        .interact()
        .map_err(|e| miette::miette!("prompt: {e}"))?;

    let site_default = existing.map(|e| e.site_url.clone());
    let entry = match choice {
        0 => {
            eprintln!(
                "\n  OAuth app: https://developer.atlassian.com/console/myapps/\n  \
                 • Set the Callback URL to exactly: http://127.0.0.1:<port>/callback\n  \
                 • Use an app dedicated to Lorekeeper — Atlassian rotates refresh tokens, so\n    \
                   sharing a grant with another tool invalidates both on every run.\n"
            );
            let (client_id, client_secret) = match existing.map(|e| &e.auth) {
                Some(AtlassianAuthMethod::Oauth {
                    client_id,
                    client_secret,
                    ..
                }) => {
                    let id = input("  client_id", Some(client_id))?;
                    // Keeping the stored secret is the DEFAULT, and re-entry is opt-in.
                    // A masked prompt can silently drop characters from a long pasted
                    // secret, and the result is invisible: the browser consent still
                    // succeeds (it never sees the secret) and only the token exchange
                    // fails, with a generic `access_denied` that points nowhere near the
                    // real cause. Not asking is the fix.
                    let sec = if confirm("  replace the stored client_secret?", false)? {
                        secret("  client_secret", None)?
                    } else {
                        client_secret.clone()
                    };
                    (id, sec)
                }
                _ => (
                    input("  client_id", None)?,
                    secret("  client_secret", None)?,
                ),
            };
            // Fingerprint what will actually be sent. A truncated paste is otherwise
            // undetectable by eye, and this is the one place to catch it before spending a
            // browser round-trip on a request that cannot succeed.
            eprintln!(
                "  using client_secret: {} chars, fingerprint {}",
                client_secret.len(),
                secret_fingerprint(&client_secret)
            );
            let port: u16 = input(
                "  callback port (must match the app's registered Callback URL)",
                Some(&lk_source::ATLASSIAN_REDIRECT_PORT.to_string()),
            )?
            .trim()
            .parse()
            .map_err(|e| miette::miette!("callback port: {e}"))?;

            // An app authorizes only the scopes registered on it, so asking Confluence
            // scopes of a Jira-only app fails the ENTIRE consent with "scopes that have not
            // been added to the app" — nothing partial is granted. Orgs commonly register one
            // app per product, so this must be selectable rather than assumed.
            //
            // The default follows the instance name because a wrong default here is not a
            // mild inconvenience: it sends the user through a browser round-trip that can
            // only fail. Naming an instance `confluence` is a clear statement of intent.
            // Each label rides with the value it names, and the default is a VALUE whose
            // position is looked up, so the offered order carries no meaning.
            const PRODUCT_CHOICES: [(lk_source::Products, &str); 3] = [
                (lk_source::Products::Both, "both"),
                (lk_source::Products::Jira, "Jira only"),
                (lk_source::Products::Confluence, "Confluence only"),
            ];
            let default = default_products(&name);
            let default_index = PRODUCT_CHOICES
                .iter()
                .position(|(candidate, _)| *candidate == default)
                .expect("every Products default is an offered choice");
            eprintln!(
                "  Pick ONLY the products this app actually has API access to — requesting a \
                 scope\n  the app lacks fails the whole authorization."
            );
            let products = PRODUCT_CHOICES[dialoguer::Select::new()
                .with_prompt("  which products does this app have API access to?")
                .items(PRODUCT_CHOICES.map(|(_, label)| label))
                .default(default_index)
                .interact()
                .map_err(|e| miette::miette!("prompt: {e}"))?]
            .0;

            // The default is Lorekeeper's least-privilege read set, but it is only a
            // default: an app grants exactly the scopes registered on it, and some refuse a
            // request for a strict SUBSET of that registration — the consent screen succeeds
            // and the token exchange is then denied on policy, which is near-impossible to
            // read from the error alone. Editing this to match the app's Permissions page is
            // the fix. Lorekeeper only ever issues reads, whatever the grant permits.
            eprintln!(
                "  Scopes default to the read-only set Lorekeeper needs. If the token \
                 exchange is denied,\n  one cause is that this is a strict subset of the \
                 app's registration — replace it with\n  the FULL registered list \
                 (developer.atlassian.com → your app → Permissions). A mistyped\n  client \
                 secret is refused the same way."
            );
            let scopes: Vec<String> = input(
                "  scopes (space-separated)",
                Some(&products.default_scopes().join(" ")),
            )?
            .split_whitespace()
            .map(str::to_string)
            .collect();

            let http = lk_source::build_http_client().map_err(|e| miette::miette!("{e}"))?;
            let grant =
                lk_source::build_atlassian_grant(&http, &client_id, &client_secret, port, &scopes)
                    .await
                    .map_err(|e| miette::miette!("{e}"))?;

            // One account often reaches several tenants; binding the vault to whichever the
            // API listed first would be a silent, wrong choice.
            let site = if grant.sites.len() == 1 {
                &grant.sites[0]
            } else {
                let labels: Vec<String> = grant
                    .sites
                    .iter()
                    .map(|s| format!("{} ({})", s.site_url, s.cloud_id))
                    .collect();
                let picked = dialoguer::Select::new()
                    .with_prompt("  which site should this instance read?")
                    .items(&labels)
                    .default(0)
                    .interact()
                    .map_err(|e| miette::miette!("prompt: {e}"))?;
                &grant.sites[picked]
            };
            eprintln!("  ✓ authorized {} ({})", site.site_url, site.cloud_id);
            AtlassianCredentials {
                site_url: site.site_url.clone(),
                auth: AtlassianAuthMethod::Oauth {
                    client_id,
                    client_secret,
                    refresh_token: grant.refresh_token.clone(),
                    cloud_id: site.cloud_id.clone(),
                },
            }
        }
        1 => {
            // As in the scoped branch: the account email carries across a switch, the token
            // does not — the two shapes are refused in each other's place.
            let email_default = match existing.map(|e| &e.auth) {
                Some(
                    AtlassianAuthMethod::ApiToken { email, .. }
                    | AtlassianAuthMethod::ScopedToken { email, .. },
                ) => Some(email.clone()),
                _ => None,
            };
            let token_default = match existing.map(|e| &e.auth) {
                Some(AtlassianAuthMethod::ApiToken { api_token, .. }) => Some(api_token.clone()),
                _ => None,
            };
            AtlassianCredentials {
                site_url: input(
                    "  site_url (https://org.atlassian.net)",
                    site_default.as_deref(),
                )?,
                auth: AtlassianAuthMethod::ApiToken {
                    email: input("  email", email_default.as_deref())?,
                    api_token: secret("  api_token", token_default.as_deref())?,
                },
            }
        }
        2 => {
            // The email names the account either way, but the TOKEN is offered back only by
            // a scoped entry. Keeping a classic one on "enter to keep" would store it in the
            // gateway variant, where it is refused for being the wrong shape — the switch
            // is made precisely because the other token is the one that does not work.
            let email_default = match existing.map(|e| &e.auth) {
                Some(
                    AtlassianAuthMethod::ScopedToken { email, .. }
                    | AtlassianAuthMethod::ApiToken { email, .. },
                ) => Some(email.clone()),
                _ => None,
            };
            let token_default = match existing.map(|e| &e.auth) {
                Some(AtlassianAuthMethod::ScopedToken { api_token, .. }) => Some(api_token.clone()),
                _ => None,
            };
            // Both gateway methods carry one, so switching between them keeps it.
            let cloud_default = match existing.map(|e| &e.auth) {
                Some(
                    AtlassianAuthMethod::ScopedToken { cloud_id, .. }
                    | AtlassianAuthMethod::Oauth { cloud_id, .. },
                ) => Some(cloud_id.clone()),
                _ => None,
            };
            eprintln!(
                "\n  Scoped token: https://id.atlassian.com/manage-profile/security/api-tokens\n  \
                 • Grant it the same scopes an app would need for the products you read.\n  \
                 • A CLASSIC token does not work here — the gateway honors only a scoped one.\n  \
                 • cloud_id is served at https://<your-site>/_edge/tenant_info\n"
            );
            AtlassianCredentials {
                site_url: input(
                    "  site_url (https://org.atlassian.net)",
                    site_default.as_deref(),
                )?,
                auth: AtlassianAuthMethod::ScopedToken {
                    email: input("  email", email_default.as_deref())?,
                    api_token: secret("  api_token (scoped)", token_default.as_deref())?,
                    cloud_id: input("  cloud_id", cloud_default.as_deref())?,
                },
            }
        }
        _ => {
            let token_default = match existing.map(|e| &e.auth) {
                Some(AtlassianAuthMethod::PersonalAccessToken { token }) => Some(token.clone()),
                _ => None,
            };
            eprintln!(
                "  Include the context path if your instance has one \
                 (e.g. https://wiki.corp/confluence)."
            );
            AtlassianCredentials {
                site_url: input("  site_url", site_default.as_deref())?,
                auth: AtlassianAuthMethod::PersonalAccessToken {
                    token: secret("  personal access token", token_default.as_deref())?,
                },
            }
        }
    };

    creds.atlassian.insert(name, entry);
    Ok(())
}

/// Resolve the Google refresh token: keep an existing one, re-authorize in the browser to
/// mint a fresh one, or paste one manually. A refresh token isn't shown in the Cloud
/// Console, so the browser flow is the path most users need.
async fn google_refresh_token(
    existing: Option<&GoogleCredentials>,
    client_id: &str,
    client_secret: &str,
) -> miette::Result<String> {
    if let Some(g) = existing.filter(|g| !g.refresh_token.is_empty())
        && !confirm(
            "  re-authorize in the browser for a new refresh_token? (No = keep existing)",
            false,
        )?
    {
        return Ok(g.refresh_token.clone());
    }

    if confirm(
        "  authorize in the browser now to mint a refresh_token? (No = paste one)",
        true,
    )? {
        let http = lk_source::build_http_client().map_err(|e| miette::miette!("{e}"))?;
        build_google_refresh_token(&http, client_id, client_secret)
            .await
            .map_err(|e| miette::miette!("{e}"))
    } else {
        // Manual paste — required (no existing value to fall back to here).
        secret("  refresh_token", None)
    }
}

/// Short, non-reversible fingerprint of a secret, safe to print.
///
/// Enough to tell two secrets apart — and to spot a truncated paste when compared against
/// the value the operator expects — while revealing nothing usable.
fn secret_fingerprint(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex()[..12].to_string()
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

/// Which Atlassian products an instance named `name` most likely has API access to.
///
/// A wrong answer here is not a mild inconvenience: requesting a scope the app lacks fails the
/// ENTIRE consent, so the user completes a browser round-trip that could only fail. Naming an
/// instance `confluence` is a clear statement of intent; naming it after both, or after neither,
/// is not, so that case asks for everything and lets the prompt narrow it.
fn default_products(name: &str) -> lk_source::Products {
    let lowered = name.to_lowercase();
    match (lowered.contains("jira"), lowered.contains("confluence")) {
        (true, false) => lk_source::Products::Jira,
        (false, true) => lk_source::Products::Confluence,
        _ => lk_source::Products::Both,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_source::Products;

    #[test]
    fn an_instance_named_after_one_product_defaults_to_it() {
        assert_eq!(default_products("jira"), Products::Jira);
        assert_eq!(default_products("Company Confluence"), Products::Confluence);
        assert_eq!(default_products("JIRA-PROD"), Products::Jira);
    }

    #[test]
    fn a_name_that_settles_nothing_offers_everything() {
        // Naming both, or neither, is not a statement about which one the app can reach.
        assert_eq!(default_products("jira-and-confluence"), Products::Both);
        assert_eq!(default_products("atlassian"), Products::Both);
        assert_eq!(default_products(""), Products::Both);
    }
}
