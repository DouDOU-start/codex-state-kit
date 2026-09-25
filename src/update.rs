use anyhow::{bail, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const RELEASES_URL: &str = "https://github.com/DouDOU-start/codex-state-kit/releases/latest";
const RELEASE_API: &str =
    "https://api.github.com/repos/DouDOU-start/codex-state-kit/releases/latest";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub tag: Option<String>,
    pub available: bool,
    pub release_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn parse_version(tag: &str) -> Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).context("发布版本号格式无效")
}

// Never accept an arbitrary URL from the renderer or release response.
pub fn release_url(tag: Option<&str>) -> Result<String> {
    let Some(tag) = tag else {
        return Ok(RELEASES_URL.into());
    };
    parse_version(tag)?;
    let mut url = url::Url::parse("https://github.com/DouDOU-start/codex-state-kit/releases/tag/")?;
    url.path_segments_mut()
        .expect("fixed HTTPS URL")
        .pop_if_empty()
        .push(tag);
    Ok(url.into())
}

fn evaluate_release(current: &str, release: Option<Release>) -> Result<UpdateInfo> {
    let local = parse_version(current)?;
    let mut info = UpdateInfo {
        current_version: current.into(),
        latest_version: None,
        tag: None,
        available: false,
        release_url: RELEASES_URL.into(),
    };
    if let Some(release) = release {
        if release.draft || release.prerelease {
            return Ok(info);
        }
        let latest = parse_version(&release.tag_name)?;
        if !latest.pre.is_empty() {
            return Ok(info);
        }
        info.available = latest.cmp_precedence(&local).is_gt();
        info.latest_version = Some(latest.to_string());
        info.release_url = release_url(Some(&release.tag_name))?;
        info.tag = Some(release.tag_name);
    }
    Ok(info)
}

pub async fn check_update(current: &str) -> Result<UpdateInfo> {
    // Update checks use the default network; the business proxy and token route are independent.
    let client = crate::tls::http_client_builder()
        .user_agent(crate::identity::VmIdentity::runtime_user_agent())
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    fetch_release(&client, current, RELEASE_API).await
}

async fn fetch_release(
    client: &reqwest::Client,
    current: &str,
    endpoint: &str,
) -> Result<UpdateInfo> {
    let response = client
        .get(endpoint)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("无法检查更新，请检查网络或稍后重试")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return evaluate_release(current, None);
    }
    if response.status() == reqwest::StatusCode::FORBIDDEN
        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        bail!("GitHub 暂时限制更新检查，请稍后重试或直接查看发布页面");
    }
    let release: Release = response
        .error_for_status()
        .context("更新服务暂时不可用")?
        .json()
        .await
        .context("无法读取发布版本信息")?;
    evaluate_release(current, Some(release))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_updater_key_verifies_signature_and_rejects_tampering() {
        use base64::Engine;
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../src-tauri/tauri.conf.json")).unwrap();
        let public_key = config["plugins"]["updater"]["pubkey"].as_str().unwrap();
        let key_text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(public_key)
                .unwrap(),
        )
        .unwrap();
        let signature_text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(include_str!("../tools/fixtures/updater-signing-test.txt.sig").trim())
                .unwrap(),
        )
        .unwrap();
        let key = minisign_verify::PublicKey::decode(&key_text).unwrap();
        let signature = minisign_verify::Signature::decode(&signature_text).unwrap();
        let payload = include_bytes!("../tools/fixtures/updater-signing-test.txt");
        key.verify(payload, &signature, true).unwrap();
        let mut tampered = payload.to_vec();
        tampered[0] ^= 1;
        assert!(key.verify(&tampered, &signature, true).is_err());
    }

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
        }
    }

    #[test]
    fn compares_versions_by_semver() {
        for (local, remote, available) in [
            ("0.0.4", "v0.0.5", true),
            ("0.0.4", "v0.0.4", false),
            ("0.0.5", "v0.0.4", false),
            ("0.9.0", "v0.10.0", true),
            ("0.0.5-beta.1", "v0.0.5", true),
            ("0.0.4+local", "v0.0.4+build", false),
        ] {
            assert_eq!(
                evaluate_release(local, Some(release(remote)))
                    .unwrap()
                    .available,
                available
            );
        }
    }

    #[test]
    fn ignores_unpublished_and_prerelease_versions() {
        let mut draft = release("v9.0.0");
        draft.draft = true;
        let mut pre = release("v9.0.0");
        pre.prerelease = true;
        for value in [None, Some(draft), Some(pre), Some(release("v9.0.0-rc.1"))] {
            let info = evaluate_release("0.0.4", value).unwrap();
            assert!(!info.available);
            assert!(info.latest_version.is_none());
        }
        assert!(evaluate_release("0.0.4", Some(release("garbage"))).is_err());
    }

    #[test]
    fn release_links_are_confined_to_this_repository() {
        assert_eq!(
            release_url(Some("v0.0.5")).unwrap(),
            "https://github.com/DouDOU-start/codex-state-kit/releases/tag/v0.0.5"
        );
        for tag in [
            "https://evil.example",
            "../../other",
            "v1.2.3?redirect=evil",
            "",
        ] {
            assert!(release_url(Some(tag)).is_err());
        }
    }

    #[tokio::test]
    async fn handles_release_api_responses() {
        for (status, body, expected) in [
            (
                200,
                r#"{"tag_name":"v0.0.5","draft":false,"prerelease":false}"#,
                Some(true),
            ),
            (404, "", Some(false)),
            (403, "", None),
            (429, "", None),
            (500, "", None),
            (200, "not json", None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                let router = axum::Router::new().fallback(move || async move {
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                });
                axum::serve(listener, router).await.unwrap();
            });
            let client = crate::tls::http_client_builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let result = fetch_release(&client, "0.0.4", &endpoint).await;
            task.abort();
            match expected {
                Some(available) => assert_eq!(result.unwrap().available, available),
                None => assert!(result.is_err()),
            }
        }
    }
}
