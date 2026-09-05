use anyhow::{Context, Result};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::PortfolioConfig;

#[cfg(test)]
mod tests {
    #[test]
    fn mailbox_parses_bare_address() {
        let addr = "srknysldg@gmail.com";
        let mb: Result<lettre::message::Mailbox, _> = addr.parse();
        assert!(mb.is_ok(), "lettre rejected a bare email address: {mb:?}");
    }

    #[test]
    fn mailbox_rejects_empty_string() {
        let mb: Result<lettre::message::Mailbox, _> = "".parse();
        assert!(mb.is_err(), "lettre should reject an empty string");
    }
}

/// Returns `true` if the email was sent, `false` if credentials are not configured (skipped).
pub async fn send_alert(cfg: &PortfolioConfig, subject: &str, body: &str) -> Result<bool> {
    let missing: Vec<&str> = [
        cfg.smtp_from.is_empty().then_some("SMTP_FROM"),
        cfg.smtp_user.is_empty().then_some("SMTP_USER"),
        cfg.smtp_password.is_empty().then_some("SMTP_PASSWORD"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !missing.is_empty() {
        tracing::warn!(
            "{} not set in .env — skipping alert: {subject}",
            missing.join(", ")
        );
        return Ok(false);
    }

    let email = Message::builder()
        .from(
            cfg.smtp_from
                .parse()
                .context("invalid SMTP_FROM address")?,
        )
        .to(cfg
            .alert_email
            .parse()
            .context("invalid ALERT_EMAIL address")?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .context("failed to build email message")?;

    let creds = Credentials::new(cfg.smtp_user.clone(), cfg.smtp_password.clone());

    let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
        .context("failed to create SMTP transport")?
        .port(cfg.smtp_port)
        .credentials(creds)
        .build();

    // Bounded: this is awaited inline on the watcher's monitor loop (alerts, trade and
    // adoption emails), so a hung SMTP session would stall the trailing stop.
    tokio::time::timeout(
        std::time::Duration::from_secs(cfg.alert_email_timeout_secs),
        transport.send(email),
    )
    .await
    .map_err(|_| anyhow::anyhow!("email send timed out after {}s", cfg.alert_email_timeout_secs))?
    .context("failed to send email")?;

    Ok(true)
}
