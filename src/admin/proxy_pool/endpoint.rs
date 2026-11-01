//! 代理地址：结构化字段、解析与规范化。

use serde::{Deserialize, Serialize};

/// 支持的代理协议（reqwest 开启 `socks` feature 后全部可用）
const PROXY_PROTOCOLS: [&str; 6] = ["http", "https", "socks4", "socks4a", "socks5", "socks5h"];

/// 代理地址的结构化表示。规范化后的值可直接比较，作为去重与关联的身份。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyEndpoint {
    pub protocol: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("protocol", &self.protocol)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

impl ProxyEndpoint {
    /// 解析 `协议://[用户名[:密码]@]主机:端口`，用户名和密码按百分号编码解码。
    ///
    /// 错误信息不回显原始地址，避免把其中的密码写进日志或响应。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let url = reqwest::Url::parse(raw.trim())
            .map_err(|_| "代理地址格式无效，应为 协议://[用户名:密码@]主机:端口".to_string())?;
        let host = url.host_str().ok_or("代理地址缺少主机")?;
        let port = url.port_or_known_default().ok_or("代理地址缺少端口号")?;
        let decode = |s: &str| {
            urlencoding::decode(s)
                .map(|v| v.into_owned())
                .unwrap_or_else(|_| s.to_string())
        };
        Self {
            protocol: url.scheme().to_string(),
            host: host.to_string(),
            port,
            username: Some(decode(url.username())),
            password: url.password().map(decode),
        }
        .normalize()
    }

    /// 规范化并校验：协议转小写且受支持，主机非空，端口非零；空的用户名或密码视为未设置，
    /// 有密码时必须有用户名。
    pub fn normalize(mut self) -> Result<Self, String> {
        self.protocol = self.protocol.trim().to_ascii_lowercase();
        if !PROXY_PROTOCOLS.contains(&self.protocol.as_str()) {
            return Err(format!(
                "不支持的代理协议 {:?}，可选: {}",
                self.protocol,
                PROXY_PROTOCOLS.join(" / ")
            ));
        }
        let host = self
            .host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']');
        if host.is_empty() {
            return Err("代理主机不能为空".to_string());
        }
        if host
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '?' | '#'))
        {
            return Err("代理主机包含非法字符".to_string());
        }
        if host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("代理主机格式无效".to_string());
        }
        self.host = host.to_ascii_lowercase();
        if self.port == 0 {
            return Err("代理端口必须在 1-65535 之间".to_string());
        }
        let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.username = clean(self.username);
        self.password = clean(self.password);
        if self.password.is_some() && self.username.is_none() {
            return Err("填写代理密码时必须同时填写用户名".to_string());
        }
        Ok(self)
    }

    /// `主机:端口`，IPv6 主机加方括号。
    pub fn host_port(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// 不含认证信息的展示地址，如 `socks5://1.2.3.4:1080`。
    pub fn display_url(&self) -> String {
        format!("{}://{}", self.protocol, self.host_port())
    }

    /// 含认证信息的完整地址，供 reqwest 使用；用户名和密码做百分号编码。
    pub fn full_url(&self) -> String {
        let userinfo = match (&self.username, &self.password) {
            (Some(user), Some(pass)) => format!(
                "{}:{}@",
                urlencoding::encode(user),
                urlencoding::encode(pass)
            ),
            (Some(user), None) => format!("{}@", urlencoding::encode(user)),
            _ => String::new(),
        };
        format!("{}://{}{}", self.protocol, userinfo, self.host_port())
    }

    /// 未填写名称时使用的默认名称
    pub fn default_name(&self) -> String {
        self.host_port()
    }
}
