use lettre::message::{header::ContentType, Attachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    pub from: String,
}

pub async fn send_pdf(
    cfg: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
    pdf_filename: &str,
    pdf_bytes: Vec<u8>,
) -> anyhow::Result<()> {
    let from = cfg
        .from
        .parse()
        .map_err(|e| anyhow::anyhow!("from address parse: {e}"))?;
    let to_addr = to
        .parse()
        .map_err(|e| anyhow::anyhow!("recipient parse: {e}"))?;

    let pdf_ct: ContentType = "application/pdf"
        .parse()
        .map_err(|e| anyhow::anyhow!("pdf content-type: {e}"))?;

    let email = Message::builder()
        .from(from)
        .to(to_addr)
        .subject(subject)
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(body.to_string()))
                .singlepart(Attachment::new(pdf_filename.to_string()).body(pdf_bytes, pdf_ct)),
        )?;

    let creds = Credentials::new(cfg.user.clone(), cfg.pass.clone());
    let mailer: AsyncSmtpTransport<Tokio1Executor> =
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)?
            .port(cfg.port)
            .credentials(creds)
            .build();

    mailer.send(email).await?;
    Ok(())
}
