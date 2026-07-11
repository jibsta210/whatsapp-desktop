//! Contact information feature.
//!
//! Profile picture types are defined in `wacore::iq::contacts`.
//! Usync types are defined in `wacore::iq::usync`.

use crate::client::Client;
use crate::request::IqError;
use anyhow::Result;
use log::debug;
use std::collections::HashMap;
use wacore::iq::contacts::{ProfilePictureSpec, ProfilePictureType};
use wacore::iq::usync::{ContactInfoSpec, IsOnWhatsAppSpec, UserInfoSpec};
use wacore_binary::jid::{Jid, JidExt};

// Re-export types from wacore
pub use wacore::iq::contacts::ProfilePicture;
pub use wacore::iq::usync::{ContactInfo, IsOnWhatsAppResult, UserInfo};

pub struct Contacts<'a> {
    client: &'a Client,
}

impl<'a> Contacts<'a> {
    pub(crate) fn new(client: &'a Client) -> Self {
        Self { client }
    }

    pub async fn is_on_whatsapp(&self, phones: &[&str]) -> Result<Vec<IsOnWhatsAppResult>> {
        if phones.is_empty() {
            return Ok(Vec::new());
        }

        debug!("is_on_whatsapp: checking {} numbers", phones.len());

        let request_id = self.client.generate_request_id();
        let phone_strings: Vec<String> = phones.iter().map(|s| s.to_string()).collect();
        let spec = IsOnWhatsAppSpec::new(phone_strings, request_id);

        Ok(self.client.execute(spec).await?)
    }

    pub async fn get_info(&self, phones: &[&str]) -> Result<Vec<ContactInfo>> {
        if phones.is_empty() {
            return Ok(Vec::new());
        }

        debug!("get_info: fetching info for {} numbers", phones.len());

        let request_id = self.client.generate_request_id();
        let phone_strings: Vec<String> = phones.iter().map(|s| s.to_string()).collect();
        let spec = ContactInfoSpec::new(phone_strings, request_id);

        Ok(self.client.execute(spec).await?)
    }

    pub async fn get_profile_picture(
        &self,
        jid: &Jid,
        preview: bool,
    ) -> Result<Option<ProfilePicture>> {
        debug!(
            "get_profile_picture: fetching {} picture for {}",
            if preview { "preview" } else { "full" },
            jid
        );

        let picture_type = if preview {
            ProfilePictureType::Preview
        } else {
            ProfilePictureType::Full
        };
        let mut spec = ProfilePictureSpec::new(jid, picture_type);

        // Include tctoken for user JIDs (skip groups, newsletters)
        if !jid.is_group()
            && !jid.is_newsletter()
            && let Some(token) = self.client.lookup_tc_token_for_jid(jid).await
        {
            spec = spec.with_tc_token(token);
        }

        match self.client.execute(spec).await {
            Ok(pic) => Ok(pic),
            // 404/401 = no profile picture (or not authorized to see it).
            // WhatsApp server returns type="error" IQ for these cases.
            Err(IqError::ServerError { code, .. }) if code == 404 || code == 401 => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get_user_info(&self, jids: &[Jid]) -> Result<HashMap<Jid, UserInfo>> {
        if jids.is_empty() {
            return Ok(HashMap::new());
        }

        debug!("get_user_info: fetching info for {} JIDs", jids.len());

        let request_id = self.client.generate_request_id();
        let spec = UserInfoSpec::new(jids.to_vec(), request_id);

        Ok(self.client.execute(spec).await?)
    }
}

impl Client {
    pub fn contacts(&self) -> Contacts<'_> {
        Contacts::new(self)
    }

    /// Resolve phone numbers to their LIDs via a ContactInfoSpec usync and
    /// persist every discovered phone↔LID pair into the core LID-PN cache.
    ///
    /// Unlike `get_user_devices` (DeviceListSpec), ContactInfoSpec requests the
    /// `<lid/>` sidecar, so the server actually returns a `<lid val="…"/>` child
    /// for each phone-keyed user — the only usync direction that yields a mapping.
    /// The desktop prewarm sweep calls this so an incoming LID resolves to the
    /// right saved contact instead of minting a phantom "+<lid digits>" chat.
    ///
    /// `phones` are bare digit strings (no `@` / `+`); the spec adds the `+`.
    /// Returns the number of new (lid, phone) mappings learned.
    pub async fn resolve_contact_lids(&self, phones: &[String]) -> anyhow::Result<usize> {
        if phones.is_empty() {
            return Ok(0);
        }

        let request_id = self.generate_request_id();
        let spec = ContactInfoSpec::new(phones.to_vec(), request_id);
        let infos = self.execute(spec).await?;

        let mut learned = 0usize;
        for info in infos {
            // Only phone-keyed results carry a usable LID sidecar. `jid.user` is
            // the phone digits; `lid.user` is the opaque LID user part.
            if let Some(lid) = info.lid {
                let phone_user = info.jid.user.as_str();
                let lid_user = lid.user.as_str();
                if phone_user.is_empty() || lid_user.is_empty() {
                    continue;
                }
                // Reachable because this method lives inside the whatsapp-rust crate.
                if let Err(e) = self
                    .add_lid_pn_mapping(lid_user, phone_user, wacore::types::LearningSource::Usync)
                    .await
                {
                    debug!("resolve_contact_lids: persist {lid_user}↔{phone_user} failed: {e:#}");
                    continue;
                }
                learned += 1;
            }
        }

        Ok(learned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contact_info_struct() {
        let jid: Jid = "1234567890@s.whatsapp.net"
            .parse()
            .expect("test JID should be valid");
        let lid: Jid = "12345678@lid".parse().expect("test JID should be valid");

        let info = ContactInfo {
            jid: jid.clone(),
            lid: Some(lid.clone()),
            is_registered: true,
            is_business: false,
            status: Some("Hey there!".to_string()),
            picture_id: Some(123456789),
        };

        assert!(info.is_registered);
        assert!(!info.is_business);
        assert_eq!(info.status, Some("Hey there!".to_string()));
        assert_eq!(info.picture_id, Some(123456789));
        assert!(info.lid.is_some());
    }

    #[test]
    fn test_profile_picture_struct() {
        let pic = ProfilePicture {
            id: "123456789".to_string(),
            url: "https://example.com/pic.jpg".to_string(),
            direct_path: Some("/v/pic.jpg".to_string()),
            hash: None,
        };

        assert_eq!(pic.id, "123456789");
        assert_eq!(pic.url, "https://example.com/pic.jpg");
        assert!(pic.direct_path.is_some());
    }

    #[test]
    fn test_is_on_whatsapp_result_struct() {
        let jid: Jid = "1234567890@s.whatsapp.net"
            .parse()
            .expect("test JID should be valid");
        let result = IsOnWhatsAppResult {
            jid,
            is_registered: true,
        };

        assert!(result.is_registered);
    }
}
