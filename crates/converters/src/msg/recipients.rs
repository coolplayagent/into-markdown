use super::budget::{MsgBudget, malformed};
use super::ole::Storage;
use super::properties::{Properties, PropertyScope};
use into_markdown_core::{ConversionError, ErrorPolicy};

const PR_SENT_REPRESENTING_NAME: u16 = 0x0042;
const PR_SENT_REPRESENTING_EMAIL_ADDRESS: u16 = 0x0065;
const PR_SENDER_NAME: u16 = 0x0c1a;
const PR_SENDER_EMAIL_ADDRESS: u16 = 0x0c1f;
const PR_RECIPIENT_TYPE: u16 = 0x0c15;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_ADDRTYPE: u16 = 0x3002;
const PR_EMAIL_ADDRESS: u16 = 0x3003;
const PR_SMTP_ADDRESS: u16 = 0x39fe;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecipientKind {
    To,
    Cc,
    Bcc,
}

impl RecipientKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::To => "To",
            Self::Cc => "Cc",
            Self::Bcc => "Bcc",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Mailbox {
    pub(super) display: Option<String>,
    pub(super) address: Option<String>,
    pub(super) source: String,
}

impl Mailbox {
    pub(super) fn formatted(&self) -> String {
        match (self.display.as_deref(), self.address.as_deref()) {
            (Some(display), Some(address)) if display != address => {
                format!("{display} <{address}>")
            }
            (_, Some(address)) => address.to_owned(),
            (Some(display), None) => display.to_owned(),
            (None, None) => String::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Recipient {
    pub(super) kind: RecipientKind,
    pub(super) mailbox: Mailbox,
}

pub(super) fn sender(properties: &Properties) -> Result<Option<Mailbox>, ConversionError> {
    let display =
        properties.text(PR_SENDER_NAME).or_else(|| properties.text(PR_SENT_REPRESENTING_NAME));
    let address = properties
        .text(PR_SMTP_ADDRESS)
        .or_else(|| properties.text(PR_SENDER_EMAIL_ADDRESS))
        .or_else(|| properties.text(PR_SENT_REPRESENTING_EMAIL_ADDRESS));
    mailbox(
        display,
        address,
        properties
            .source(PR_SMTP_ADDRESS)
            .or_else(|| properties.source(PR_SENDER_EMAIL_ADDRESS))
            .or_else(|| properties.source(PR_SENT_REPRESENTING_EMAIL_ADDRESS)),
    )
}

pub(super) fn parse_all(
    root: Storage<'_>,
    codepage: u32,
    budget: &mut MsgBudget<'_>,
) -> Result<Vec<Recipient>, ConversionError> {
    let mut storages = root
        .storages()
        .filter(|storage| storage.name().starts_with("__recip_version1.0_#"))
        .collect::<Vec<_>>();
    storages.sort_by_key(Storage::name);
    let mut output = Vec::with_capacity(storages.len());
    for storage in storages {
        budget.entry()?;
        let properties = Properties::parse(storage, PropertyScope::Object, codepage, budget)?;
        let raw_kind = properties
            .integer(PR_RECIPIENT_TYPE)
            .ok_or_else(|| malformed(storage.path(), "recipient has no PR_RECIPIENT_TYPE"))?;
        let kind = match raw_kind {
            1 => RecipientKind::To,
            2 => RecipientKind::Cc,
            3 => RecipientKind::Bcc,
            _ => {
                return Err(malformed(
                    storage.path(),
                    format!("invalid recipient type {raw_kind}"),
                ));
            }
        };
        let address =
            properties.text(PR_SMTP_ADDRESS).or_else(|| properties.text(PR_EMAIL_ADDRESS));
        let mut mailbox = mailbox(
            properties.text(PR_DISPLAY_NAME),
            address,
            properties.source(PR_SMTP_ADDRESS).or_else(|| properties.source(PR_EMAIL_ADDRESS)),
        )?;
        if mailbox.is_none()
            && budget.options().error_policy == ErrorPolicy::BestEffort
            && let Some(display) =
                properties.text(PR_DISPLAY_NAME).map(str::trim).filter(|value| !value.is_empty())
        {
            if !safe_header(display) {
                return Err(malformed(storage.path(), "mailbox contains unsafe controls"));
            }
            let source = properties.source(PR_DISPLAY_NAME).unwrap_or("msg/recipient");
            budget.warning(
                "msg.recipientAddressMissing",
                "recipient display name was retained without inventing an address",
                source,
            );
            mailbox = Some(Mailbox {
                display: Some(display.to_owned()),
                address: None,
                source: source.to_owned(),
            });
        }
        let mailbox =
            mailbox.ok_or_else(|| malformed(storage.path(), "recipient has no address"))?;
        if properties.text(PR_ADDRTYPE).is_some_and(|value| !safe_header(value)) {
            return Err(malformed(
                storage.path(),
                "recipient address type contains unsafe controls",
            ));
        }
        output.push(Recipient { kind, mailbox });
    }
    Ok(output)
}

fn mailbox(
    display: Option<&str>,
    address: Option<&str>,
    source: Option<&str>,
) -> Result<Option<Mailbox>, ConversionError> {
    let Some(address) = address.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if !safe_header(address) || display.is_some_and(|value| !safe_header(value)) {
        return Err(malformed(source.unwrap_or("msg/header"), "mailbox contains unsafe controls"));
    }
    Ok(Some(Mailbox {
        display: display.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
        address: Some(address.to_owned()),
        source: source.unwrap_or("msg/__properties_version1.0").to_owned(),
    }))
}

fn safe_header(value: &str) -> bool {
    !value.chars().any(|character| {
        character == '\0'
            || character == '\r'
            || character == '\n'
            || (character.is_control() && character != '\t')
    })
}
