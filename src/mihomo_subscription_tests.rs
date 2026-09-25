use super::*;

fn one_node(raw: &str) -> ProxyNode {
    let mut nodes = parse_subscription(raw)
        .expect("subscription should parse")
        .nodes;
    assert_eq!(nodes.len(), 1);
    nodes.remove(0)
}

fn vmess_link(network: &str) -> String {
    let payload = serde_json::json!({
        "add": "vmess.example",
        "port": 443,
        "id": "11111111-1111-1111-1111-111111111111",
        "net": network,
        "host": "first.example,second.example",
        "path": "/transport",
        "ps": "edge"
    });
    format!(
        "vmess://{}",
        base64::engine::general_purpose::STANDARD.encode(payload.to_string())
    )
}

#[test]
fn accepts_all_four_base64_alphabets_and_padding_modes() {
    let raw = "trojan://secret@example.test:443#🚀";
    let engines = [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    let standard = engines[0].encode(raw);
    assert!(standard.contains('+') || standard.contains('/'));
    assert!(standard.ends_with('='));
    for engine in engines {
        assert_eq!(one_node(&engine.encode(raw)).name, "🚀");
    }
}

#[test]
fn accepts_bom_before_plain_and_encoded_subscriptions() {
    let raw = "trojan://secret@example.test:443#edge";
    assert_eq!(one_node(&format!("\u{feff}{raw}")).name, "edge");
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("\u{feff}{raw}"));
    assert_eq!(one_node(&format!("\u{feff}{encoded}")).name, "edge");
}

#[test]
fn accepts_explicit_base64_prefixes() {
    let raw = "trojan://secret@example.test:443#edge";
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    assert_eq!(one_node(&format!("base64://{encoded}")).name, "edge");
    assert_eq!(one_node(&format!("base64,{encoded}")).name, "edge");
}

#[test]
fn accepts_url_encoded_shadowsocks_base64_padding() {
    let payload = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:pass");
    assert!(payload.ends_with('='));
    let node = one_node(&format!(
        "ss://{}@example.test:8388#edge",
        payload.replace('=', "%3D")
    ));
    assert_eq!(node.spec["password"].as_str(), Some("pass"));
}

#[test]
fn accepts_url_encoded_vmess_base64_padding() {
    let mut payload = r#"{"add":"vmess.example","port":443,"id":"11111111-1111-1111-1111-111111111111","ps":"edge"}"#.to_string();
    // JSON whitespace keeps the fixture valid while ensuring padding is present.
    while payload.len() % 3 == 0 {
        payload.push(' ');
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
    assert!(encoded.ends_with('='));
    assert_eq!(
        one_node(&format!("vmess://{}", encoded.replace('=', "%3D"))).name,
        "edge"
    );
}

#[test]
fn accepts_sip002_plaintext_2022_credentials() {
    let node =
        one_node("ss://2022-blake3-aes-256-gcm:first%3Asecond%40third%3D@example.test:8388#edge");
    assert_eq!(
        node.spec["cipher"].as_str(),
        Some("2022-blake3-aes-256-gcm")
    );
    assert_eq!(node.spec["password"].as_str(), Some("first:second@third="));
}

#[test]
fn accepts_legacy_shadowsocks_base64_with_trailing_slash() {
    let payload =
        base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:pass@example.test:8388");
    let node = one_node(&format!("ss://{payload}/#edge"));
    assert_eq!(node.spec["server"].as_str(), Some("example.test"));
    assert_eq!(node.spec["password"].as_str(), Some("pass"));
}

#[test]
fn accepts_network_query_alias_for_websocket_links() {
    for scheme in ["vless", "trojan"] {
        let node = one_node(&format!(
            "{scheme}://secret@example.test:443?network=ws&host=cdn.example&path=%2Fchat"
        ));
        assert_eq!(node.spec["network"].as_str(), Some("ws"));
        assert_eq!(node.spec["ws-opts"]["path"].as_str(), Some("/chat"));
        assert_eq!(
            node.spec["ws-opts"]["headers"]["Host"].as_str(),
            Some("cdn.example")
        );
    }
}

#[test]
fn renders_h2_with_h2_options_for_vmess_and_vless() {
    let links = [
        vmess_link("h2"),
        "vless://uuid@example.test:443?type=h2&host=first.example%2Csecond.example&path=%2Ftransport".into(),
    ];
    for link in links {
        let spec = one_node(&link).spec;
        assert_eq!(spec["network"].as_str(), Some("h2"));
        assert_eq!(spec["h2-opts"]["path"].as_str(), Some("/transport"));
        assert_eq!(
            spec["h2-opts"]["host"],
            serde_yaml::Value::Sequence(vec![
                yaml_str("first.example"),
                yaml_str("second.example")
            ])
        );
        assert!(spec.get("http-opts").is_none());
    }
}

#[test]
fn keeps_http_options_distinct_from_h2_options() {
    let links = [
        vmess_link("http"),
        "vless://uuid@example.test:443?type=http&host=cdn.example&path=%2Ftransport".into(),
    ];
    for link in links {
        let spec = one_node(&link).spec;
        assert_eq!(spec["network"].as_str(), Some("http"));
        assert!(spec["http-opts"]["path"].as_sequence().is_some());
        assert!(spec["http-opts"]["headers"]["Host"].as_sequence().is_some());
        assert!(spec.get("h2-opts").is_none());
    }
}

#[test]
fn preserves_v2ray_plugin_flags_as_booleans_with_default_mode() {
    let node = one_node(
        "ss://YWVzLTI1Ni1nY206cGFzcw@example.test:8388/?plugin=v2ray-plugin%3Btls%3Bmux%3Bhost%3Dcdn.example",
    );
    let options = &node.spec["plugin-opts"];
    assert_eq!(node.spec["plugin"].as_str(), Some("v2ray-plugin"));
    assert_eq!(options["mode"].as_str(), Some("websocket"));
    assert_eq!(options["tls"].as_bool(), Some(true));
    assert_eq!(options["mux"].as_bool(), Some(true));
    assert_eq!(options["host"].as_str(), Some("cdn.example"));
}

#[test]
fn preserves_explicit_false_v2ray_plugin_flags_as_booleans() {
    let node = one_node(
        "ss://YWVzLTI1Ni1nY206cGFzcw@example.test:8388/?plugin=v2ray-plugin%3Btls%3Dfalse%3Bmux%3Dfalse",
    );
    let options = &node.spec["plugin-opts"];
    assert_eq!(options["mode"].as_str(), Some("websocket"));
    assert_eq!(options["tls"].as_bool(), Some(false));
    assert_eq!(options["mux"].as_bool(), Some(false));
}

#[test]
fn keeps_yaml_node_names_consistent_with_generated_group_references() {
    let parsed = parse_subscription(
        "proxies:\n  - name: ' alpha '\n    type: trojan\n    server: example.test\n    port: 443\n    password: secret\n",
    )
    .unwrap();
    let node = &parsed.nodes[0];
    assert_eq!(node.spec["name"].as_str(), Some(node.name.as_str()));
    let rendered: serde_yaml::Value =
        serde_yaml::from_str(&render_config(&parsed, 17891, "127.0.0.1:17892", "secret")).unwrap();
    assert_eq!(
        rendered["proxy-groups"][0]["proxies"][0],
        rendered["proxies"][0]["name"]
    );
}

#[test]
fn avoids_collision_between_node_name_and_generated_kit_group() {
    let parsed = parse_subscription(
        "proxies:\n  - name: Kit\n    type: trojan\n    server: example.test\n    port: 443\n    password: secret\n",
    )
    .unwrap();
    let rendered: serde_yaml::Value =
        serde_yaml::from_str(&render_config(&parsed, 17891, "127.0.0.1:17892", "secret")).unwrap();
    assert_ne!(
        rendered["proxies"][0]["name"],
        rendered["proxy-groups"][0]["name"]
    );
    assert_eq!(
        rendered["proxy-groups"][0]["proxies"][0],
        rendered["proxies"][0]["name"]
    );
}

#[test]
fn explains_provider_only_clash_profiles() {
    let error = match parse_subscription(
        "proxy-providers:\n  remote:\n    type: http\n    url: https://example.test/sub\n",
    ) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("provider-only profile should be rejected"),
    };
    assert!(error.contains("proxy-providers"));
    assert!(error.contains("proxies"));
}

#[test]
fn reports_line_numbers_for_unsupported_only_uri_subscriptions() {
    let error = match parse_subscription("# provider output\nssr://opaque-share-link\n") {
        Err(error) => error.to_string(),
        Ok(_) => panic!("unsupported-only URI profile should be rejected"),
    };
    assert!(error.contains("第 2 行"));
    assert!(error.contains("格式不支持"));
    assert!(!error.contains("opaque-share-link"));
}

#[test]
#[ignore = "requires MIHOMO_TEST_BINARY pointing at the pinned Mihomo v1.19.31 binary"]
fn validates_rendered_config_with_pinned_mihomo() {
    let binary = std::env::var_os("MIHOMO_TEST_BINARY")
        .expect("set MIHOMO_TEST_BINARY to the pinned Mihomo v1.19.31 binary");
    let ss_userinfo = base64::engine::general_purpose::STANDARD.encode("aes-256-gcm:secret");
    let links = [
        format!(
            "ss://{ss_userinfo}@example.test:8388/?plugin=v2ray-plugin%3Btls%3Bhost%3Dcdn.example#ss"
        ),
        vmess_link("ws"),
        "vless://11111111-1111-1111-1111-111111111111@example.test:443?type=h2&security=tls&host=first.example%2Csecond.example&path=%2Ftransport#vless-h2".into(),
        "trojan://secret@example.test:443?type=ws&sni=front.example&host=cdn.example&path=%2Fchat#trojan-ws".into(),
        "hysteria2://secret@example.test:443?sni=front.example&obfs=salamander&obfs-password=obfs-secret#hy2".into(),
        "anytls://secret@example.test:443?sni=front.example#anytls".into(),
        "tuic://11111111-1111-1111-1111-111111111111:secret@example.test:443?sni=front.example&congestion_control=bbr&udp_relay_mode=native#tuic".into(),
    ];
    let parsed = parse_subscription(&links.join("\n")).expect("share links should parse");
    assert_eq!(parsed.nodes.len(), links.len());
    let yaml = parse_subscription(
        "proxies:\n  - name: Kit\n    type: trojan\n    server: example.test\n    port: 443\n    password: secret\n  - name: alpha\n    type: trojan\n    server: example.test\n    port: 443\n    password: secret\n  - name: alpha\n    type: trojan\n    server: example.test\n    port: 443\n    password: secret\n    dialer-proxy: alpha\n",
    )
    .expect("YAML nodes should parse");
    let mut nodes = parsed.nodes;
    nodes.extend(yaml.nodes);
    let config = SubscriptionConfig {
        nodes: unique_nodes(nodes.into_iter()),
        groups: Vec::new(),
        rules: Vec::new(),
    };
    let rendered = render_config(&config, 17891, "127.0.0.1:17892", "test-secret");
    let dir = tempfile::tempdir().expect("create Mihomo test directory");
    let config_path = dir.path().join("config.yaml");
    std::fs::write(&config_path, rendered).expect("write Mihomo config");
    let output = std::process::Command::new(binary)
        .args([
            "-t",
            "-d",
            dir.path().to_str().expect("UTF-8 temp path"),
            "-f",
            config_path.to_str().expect("UTF-8 config path"),
        ])
        .output()
        .expect("run Mihomo config check");
    assert!(
        output.status.success(),
        "Mihomo v1.19.31 rejected rendered config:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
