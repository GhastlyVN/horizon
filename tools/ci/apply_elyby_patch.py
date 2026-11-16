#!/usr/bin/env python3
"""Apply the small Ely.by session/skin patch to an exact upstream Nightly source tree.

The source archive is verified before this script runs. Every edit is anchored to upstream code;
if upstream changes one of those anchors, fail rather than silently publish a partial patch.
"""
from __future__ import annotations

import sys
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one upstream anchor, found {count}")
    return text.replace(old, new, 1)


def patch_auth(path: Path) -> None:
    text = path.read_text(encoding="utf-8")
    text = replace_once(text,
        "    access_token: String,\n}\nimpl std::fmt::Debug for AuthSession",
        "    access_token: String,\n    elyby: bool,\n}\nimpl std::fmt::Debug for AuthSession",
        "AuthSession field")
    text = replace_once(text,
        "            access_token: access_token.to_owned(),\n        }\n    }\n\n    pub fn access_token(&self) -> &str {",
        "            access_token: access_token.to_owned(),\n            elyby: false,\n        }\n    }\n\n    pub fn access_token(&self) -> &str {",
        "AuthSession::new")
    text = replace_once(text,
        """        if field("user_type").is_some_and(|t| t != "msa") {
            return Err("the session is not a Microsoft account session".into());
        }
        Ok(Self {
            name: name.to_owned(),
            uuid,
            xuid: field("xuid").map(str::to_owned),
            access_token: access_token.to_owned(),
        })""",
        """        let elyby = field("user_type") == Some("elyby");
        if field("user_type").is_some_and(|t| t != "msa" && t != "elyby") {
            return Err("the session is not a supported account session".into());
        }
        Ok(Self {
            name: name.to_owned(),
            uuid,
            xuid: field("xuid").map(str::to_owned),
            access_token: access_token.to_owned(),
            elyby,
        })""",
        "AuthSession::parse")
    text = replace_once(text,
        """pub fn fetch_account_status(session: &AuthSession) -> crate::ban_notice::AccountStatus {
    use crate::ban_notice::{AccountStatus, parse_profile_actions, parse_properties};""",
        """pub fn fetch_account_status(session: &AuthSession) -> crate::ban_notice::AccountStatus {
    if session.elyby {
        return crate::ban_notice::AccountStatus::default();
    }
    use crate::ban_notice::{AccountStatus, parse_profile_actions, parse_properties};""",
        "fetch_account_status")
    text = replace_once(text,
        """fn join_server_once(session: &AuthSession, server_hash: &str) -> Result<(), JoinError> {
    let doc = discovery()?;""",
        """fn join_server_once(session: &AuthSession, server_hash: &str) -> Result<(), JoinError> {
    if session.elyby {
        return join_ely_server_once(session, server_hash);
    }
    let doc = discovery()?;""",
        "join_server_once")
    text = replace_once(text,
        """}

/// The account's chat key pair (ProfileKeyPair): the private key that signs""",
        """}

fn join_ely_server_once(session: &AuthSession, server_hash: &str) -> Result<(), JoinError> {
    match agent()
        .post("https://authserver.ely.by/session/join")
        .set("Content-Type", "application/json; charset=utf-8")
        .send_string(&join_request(session, server_hash))
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, response)) => Err(JoinError::from_response(
            status,
            &response.into_string().unwrap_or_default(),
        )),
        Err(ureq::Error::Transport(e)) => {
            log::warn!("Ely.by's session service: {e}");
            Err(JoinError::Unavailable)
        }
    }
}

/// The account's chat key pair (ProfileKeyPair): the private key that signs""",
        "Ely.by session join")
    text = replace_once(text,
        """pub fn fetch_chat_keys(session: &AuthSession) -> Result<ChatKeyPair, JoinError> {
    let doc = discovery()?;""",
        """pub fn fetch_chat_keys(session: &AuthSession) -> Result<ChatKeyPair, JoinError> {
    // An Ely.by token is not a Minecraft Services token: never send it there.
    if session.elyby {
        return Err(JoinError::Unavailable);
    }
    let doc = discovery()?;""",
        "fetch_chat_keys")
    text = replace_once(text,
        """pub fn own_textures(session: &AuthSession) -> Option<crate::skins::Textures> {
    let response = agent()""",
        """pub fn own_textures(session: &AuthSession) -> Option<crate::skins::Textures> {
    if session.elyby {
        // Ely.by exposes a Mojang-compatible profile, keyed by nickname.
        // Some skins-system deployments answer the raw /textures payload more
        // reliably than the GameProfile wrapper, so use it as a fallback.
        let name = url_encode(&session.name);
        let profile_url = format!("http://skinsystem.ely.by/profile/{name}?unsigned=false");
        let profile = agent()
            .get(&profile_url)
            .call()
            .ok()
            .and_then(|response| response.into_string().ok())
            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok());
        if let Some(profile) = profile {
            let textures = crate::skins::ely_profile_textures(&profile);
            if textures.skin.is_some() {
                return Some(textures);
            }
        }

        let textures_url = format!("http://skinsystem.ely.by/textures/{name}");
        let response = agent().get(&textures_url).call().ok()?;
        if response.status() != 200 {
            return None;
        }
        let body = response.into_string().ok()?;
        let payload: serde_json::Value = serde_json::from_str(&body).ok()?;
        return Some(crate::skins::ely_textures_response(&payload));
    }
    let response = agent()""",
        "own_textures")
    text = replace_once(text,
        """        assert_eq!(s.xuid.as_deref(), Some("25"));
    }

    #[test]
    fn join_request_like_authlib()""",
        """        assert_eq!(s.xuid.as_deref(), Some("25"));
    }

    #[test]
    fn elyby_session_file_is_accepted() {
        let session = AuthSession::parse(
            r#"{"name":"Ely","uuid":"069a79f444e94726a5befca90e38aaf5","access_token":"t","user_type":"elyby"}"#,
        )
        .unwrap();
        assert!(session.elyby);
    }

    #[test]
    fn join_request_like_authlib()""",
        "Ely.by session test")
    required = ["elyby: bool", "join_ely_server_once", "session.elyby", "elyby_session_file_is_accepted"]
    if any(marker not in text for marker in required):
        raise SystemExit("auth.rs: Ely.by patch validation failed")
    path.write_text(text, encoding="utf-8", newline="\n")


def patch_skins(path: Path) -> None:
    text = path.read_text(encoding="utf-8")
    helpers = r'''
/// Read Ely.by's Mojang-compatible /profile/{nickname} GameProfile response.
pub fn ely_profile_textures(profile: &serde_json::Value) -> Textures {
    const ELY_TEXTURE_URIS: [&str; 8] = [
        "http://ely.by/storage/skins/",
        "https://ely.by/storage/skins/",
        "http://ely.by/storage/cloaks/",
        "https://ely.by/storage/cloaks/",
        "http://ely.by/minecraft/skins/",
        "https://ely.by/minecraft/skins/",
        "http://ely.by/minecraft/cloaks/",
        "https://ely.by/minecraft/cloaks/",
    ];
    let Some((value, signature)) = parse_profile(profile).textures else {
        return Textures::default();
    };
    let allowed: Vec<&str> = TEXTURE_URIS
        .iter()
        .copied()
        .chain(ELY_TEXTURE_URIS)
        .collect();
    unpack(&value, signature, &allowed)
}

/// Parse Ely.by's raw /textures/{nickname} response, validating every texture URL.
pub fn ely_textures_response(payload: &serde_json::Value) -> Textures {
    const ELY_TEXTURE_URIS: [&str; 8] = [
        "http://ely.by/storage/skins/",
        "https://ely.by/storage/skins/",
        "http://ely.by/storage/cloaks/",
        "https://ely.by/storage/cloaks/",
        "http://ely.by/minecraft/skins/",
        "https://ely.by/minecraft/skins/",
        "http://ely.by/minecraft/cloaks/",
        "https://ely.by/minecraft/cloaks/",
    ];
    let allowed = |url: &str| {
        TEXTURE_URIS
            .iter()
            .any(|u| url.starts_with(&u.replace("{textureId}", "")))
            || ELY_TEXTURE_URIS.iter().any(|u| url.starts_with(u))
    };
    let skin_entry = payload.get("SKIN");
    let skin = skin_entry
        .and_then(|entry| entry.get("url"))
        .and_then(serde_json::Value::as_str)
        .filter(|url| allowed(url))
        .map(str::to_owned);
    let slim = skin_entry
        .and_then(|entry| entry.pointer("/metadata/model"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|model| model.eq_ignore_ascii_case("slim"));
    let cape = payload
        .get("CAPE")
        .and_then(|entry| entry.get("url"))
        .and_then(serde_json::Value::as_str)
        .filter(|url| allowed(url))
        .map(str::to_owned);
    Textures {
        skin,
        slim,
        cape,
        elytra: None,
        signed: false,
    }
}

'''
    text = replace_once(text,
        "/// The skin URL of a ",
        helpers + "/// The skin URL of a ",
        "Ely.by skin helpers")
    test = r'''
    #[test]
    fn ely_raw_textures_endpoint_accepts_documented_skin_payload() {
        let payload = serde_json::json!({
            "SKIN": {
                "url": "http://ely.by/storage/skins/test.png",
                "metadata": {"model": "slim"}
            },
            "CAPE": {"url": "https://ely.by/storage/cloaks/test.png"}
        });
        let textures = super::ely_textures_response(&payload);
        assert_eq!(
            textures.skin.as_deref(),
            Some("http://ely.by/storage/skins/test.png")
        );
        assert!(textures.slim);
        assert_eq!(
            textures.cape.as_deref(),
            Some("https://ely.by/storage/cloaks/test.png")
        );
        assert!(!textures.signed);

        let hostile = serde_json::json!({
            "SKIN": {"url": "https://evil.example/skin.png"},
            "CAPE": {"url": "https://evil.example/cape.png"}
        });
        assert_eq!(
            super::ely_textures_response(&hostile),
            super::Textures::default()
        );
    }

'''
    text = replace_once(text,
        "    #[test]\n    fn services_profile_answers_give_the_active_skin_and_cape()",
        test + "    #[test]\n    fn services_profile_answers_give_the_active_skin_and_cape()",
        "Ely.by raw skin test")
    if "pub fn ely_profile_textures" not in text or "pub fn ely_textures_response" not in text:
        raise SystemExit("skins.rs: Ely.by patch validation failed")
    path.write_text(text, encoding="utf-8", newline="\n")


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: apply_elyby_patch.py <upstream-source-root>")
    root = Path(sys.argv[1]).resolve()
    auth = root / "crates/horizon-client-render/src/auth.rs"
    skins = root / "crates/horizon-client-render/src/skins.rs"
    if not auth.is_file() or not skins.is_file():
        raise SystemExit(f"{root}: not an upstream Horizon source tree")
    patch_auth(auth)
    patch_skins(skins)
    print("Applied Ely.by session, join-server, chat-key guard, and skin fallback patches to upstream source.")


if __name__ == "__main__":
    main()
