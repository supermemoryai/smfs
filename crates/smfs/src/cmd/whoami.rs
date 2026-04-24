use anyhow::Result;
use clap::Args as ClapArgs;

use smfs_core::api::ApiClient;

use super::auth::resolve_api_key_with_source;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(long, env = "SUPERMEMORY_API_URL")]
    pub api_url: Option<String>,

    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let (api_key, source) = resolve_api_key_with_source(None, None)?;
    let api_url = args
        .api_url
        .or_else(|| smfs_core::config::credentials::load_global().and_then(|c| c.api_url))
        .unwrap_or_else(|| "https://api.supermemory.ai".to_string());

    let session = ApiClient::validate_key(&api_url, &api_key).await?;

    if args.json {
        let out = serde_json::json!({
            "org": session.org_name,
            "user_id": session.user_id,
            "user_name": session.user_name,
            "user_email": session.user_email,
            "plan": session.plan,
            "api_url": api_url,
            "key_redacted": redact(&api_key),
            "key_source": source.label(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let user_display = match (&session.user_name, &session.user_email) {
        (Some(n), Some(e)) => format!("{n} <{e}>"),
        (Some(n), None) => n.clone(),
        (None, Some(e)) => e.clone(),
        (None, None) => "<unknown>".to_string(),
    };
    println!("user:  {user_display}");
    println!(
        "id:    {}",
        session.user_id.as_deref().unwrap_or("<unknown>")
    );
    println!("org:   {}", session.org_name);
    if let Some(plan) = session.plan.as_deref() {
        println!("plan:  {plan}");
    }
    println!("api:   {api_url}");
    println!("key:   {}  (source: {})", redact(&api_key), source.label());
    Ok(())
}

fn redact(key: &str) -> String {
    if key.len() < 12 {
        return "***".to_string();
    }
    format!("{}...{}", &key[..7], &key[key.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_short_key_returns_stars() {
        assert_eq!(redact("short"), "***");
    }

    #[test]
    fn redact_long_key_keeps_prefix_and_suffix() {
        let key = "sm_77eBEqPR8tc7vv66MeUA6i_3ZfEanzRGxoGmkrfHT9iCw8iqvwSjeSofmShrkocbAkO1V9A2cZ5gqLDGPWqQ7NE";
        let r = redact(key);
        assert!(r.starts_with("sm_77eB"));
        assert!(r.ends_with("7NE"));
        assert!(r.contains("..."));
        assert!(!r.contains("MeUA"));
    }
}
