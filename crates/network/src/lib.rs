//! 共享 TCP/TLS 传输；证书校验由 rustls 完成，不提供跳过验证开关。
//! Shared TCP/TLS transport; rustls verifies certificates without an insecure bypass.

use ling_types::{DbError, ErrorCode};
use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{
        self, RootCertStore,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    },
};

/// 可拆分且可取消的双向传输；Duplex transport supporting split ownership and cancellation.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
/// 同一客户端在 TCP 与 TLS 上使用相同协议状态机。
/// The same client protocol state machine operates over TCP and TLS.
pub type Stream = Box<dyn Io>;

/// 客户端身份与信任设置；不含证书或私钥内容。
/// Client identity and trust settings containing no certificate or key contents.
#[derive(Clone, Debug)]
pub struct ClientTls {
    /// 必须匹配证书的 DNS 名称或 IP；DNS name or IP that must match the certificate.
    pub server_name: String,
    /// 为空使用公共根，指定时仅信任该 PEM；None uses public roots; Some trusts only this PEM.
    pub ca_file: Option<PathBuf>,
    /// 可选 mTLS 客户端证书链；Optional mTLS client certificate chain.
    pub cert_file: Option<PathBuf>,
    /// 必须与证书同时提供；Must accompany the client certificate.
    pub key_file: Option<PathBuf>,
}
impl ClientTls {
    /// 加载受信任根和可选客户端身份，拒绝缺失或不匹配的配置。
    /// Loads trust roots and optional client identity, rejecting missing or mismatched settings.
    pub fn connector(&self) -> Result<(TlsConnector, ServerName<'static>), DbError> {
        let mut roots = RootCertStore::empty();
        if let Some(path) = &self.ca_file {
            for certificate in certificates(path)? {
                roots.add(certificate).map_err(|_| invalid())?;
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let builder = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|_| invalid())?
            .with_root_certificates(roots);
        let config = match (&self.cert_file, &self.key_file) {
            (None, None) => builder.with_no_client_auth(),
            (Some(cert), Some(key)) => builder
                .with_client_auth_cert(certificates(cert)?, private_key(key)?)
                .map_err(|_| invalid())?,
            _ => return Err(invalid()),
        };
        let name = ServerName::try_from(self.server_name.clone()).map_err(|_| invalid())?;
        Ok((TlsConnector::from(Arc::new(config)), name))
    }
}

/// 在监听前加载服务端证书与可选客户端 CA；返回的接收器可廉价克隆。
/// Loads server identity and optional client CA before listening; the resulting acceptor is cheap to clone.
pub fn server_tls(
    cert: &Path,
    key: &Path,
    ca: Option<&Path>,
    min_version: &str,
) -> Result<TlsAcceptor, DbError> {
    let versions = match min_version {
        "tls1.2" => &[&rustls::version::TLS13, &rustls::version::TLS12][..],
        "tls1.3" => &[&rustls::version::TLS13][..],
        _ => return Err(invalid()),
    };
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .map_err(|_| invalid())?;
    let builder = if let Some(ca) = ca {
        let mut roots = RootCertStore::empty();
        for certificate in certificates(ca)? {
            roots.add(certificate).map_err(|_| invalid())?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .map_err(|_| invalid())?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let config = builder
        .with_single_cert(certificates(cert)?, private_key(key)?)
        .map_err(|_| invalid())?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// 显式指定成熟密码提供器，避免进程全局初始化竞争。
/// Explicitly selects the established provider without process-global initialization races.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}
/// PEM 文件必须包含证书，解析错误不输出原文。
/// PEM files must contain certificates; parse errors never echo source contents.
fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, DbError> {
    let mut reader = BufReader::new(File::open(path).map_err(|_| invalid())?);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    if certs.is_empty() {
        return Err(invalid());
    }
    Ok(certs)
}
/// rustls 校验证书与私钥匹配；Rustls checks certificate/key correspondence.
fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>, DbError> {
    let mut reader = BufReader::new(File::open(path).map_err(|_| invalid())?);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|_| invalid())?
        .ok_or_else(invalid)
}
/// 文件和密码学诊断不能泄露私钥；File and crypto diagnostics must not expose private keys.
fn invalid() -> DbError {
    DbError::new(
        ErrorCode::ConfigInvalid,
        "Invalid TLS identity, trust roots or protocol version",
    )
}
