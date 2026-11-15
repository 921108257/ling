package io.linger.cache;

import java.io.*;
import java.nio.file.*;
import java.security.*;
import java.security.cert.*;
import javax.net.ssl.*;

/**
 * 使用 JSSE 的信任库与可选 PKCS12 身份，不实现自定义密码学。 JSSE trust and optional PKCS12 identity without custom
 * cryptography.
 */
public final class Tls {
  private Tls() {}

  /**
   * CA 为空使用系统信任；身份库可为空，密码不会记录。 Null CA uses system trust; identity is optional and passwords are
   * never logged.
   */
  public static SSLContext context(Path ca, Path identity, char[] password)
      throws GeneralSecurityException, IOException {
    TrustManagerFactory trust =
        TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
    KeyStore roots = null;
    if (ca != null) {
      roots = KeyStore.getInstance(KeyStore.getDefaultType());
      roots.load(null, null);
      try (InputStream input = Files.newInputStream(ca)) {
        int index = 0;
        for (var certificate : CertificateFactory.getInstance("X.509").generateCertificates(input))
          roots.setCertificateEntry("ca-" + (index++), certificate);
        if (index == 0) throw new CertificateException("Empty CA file");
      }
    }
    trust.init(roots);
    KeyManager[] managers = null;
    if (identity != null) {
      KeyStore keys = KeyStore.getInstance("PKCS12");
      try (InputStream input = Files.newInputStream(identity)) {
        keys.load(input, password);
      }
      KeyManagerFactory factory =
          KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
      factory.init(keys, password);
      managers = factory.getKeyManagers();
    }
    SSLContext context = SSLContext.getInstance("TLS");
    context.init(managers, trust.getTrustManagers(), null);
    return context;
  }
}
