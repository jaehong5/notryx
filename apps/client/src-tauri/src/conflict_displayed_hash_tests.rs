use super::matches_displayed_hash;

#[test]
fn displayed_hash_verifies_exact_raw_bytes_against_protocol_digest() {
    let cases: &[(&[u8], &str, &str)] = &[
        (
            b"",
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:",
        ),
        (
            b"\xef\xbb\xbftitle\r\nsecond\r\n",
            "sha256:c66f9eb539f23b9862c7da10959affcb91c7e281686286c2b05ae2c4447f3147",
            "sha-256=:xm+etTnyO5hix9oQlZr/y5HH4oFoYobCsFrixER/MUc=:",
        ),
        (
            b"title\r\nsecond\r\n",
            "sha256:202b9702619736bf611dadfd88b4076e45d44a57f01ee27d54a09f1621674f7d",
            "sha-256=:ICuXAmGXNr9hHa39iLQHbkXUSlfwHuJ9VKCfFiFnT30=:",
        ),
        (
            b"title\nsecond\n",
            "sha256:e29725606bda5a3c94aefaf822aac5a0c75655e305afc0ea70f197163721e784",
            "sha-256=:4pclYGvaWjyUrvr4IqrFoMdWVeMFr8DqcPGXFjch54Q=:",
        ),
        (
            b"\xef\xbb\xbf",
            "sha256:f1945cd6c19e56b3c1c78943ef5ec18116907a4ca1efc40a57d48ab1db7adfc5",
            "sha-256=:8ZRc1sGeVrPBx4lD717BgRaQekyh78QKV9SKsdt638U=:",
        ),
    ];
    for (bytes, expected, protocol_header) in cases {
        assert!(notryx_protocol::verify_content_digest(protocol_header, bytes).is_ok());
        assert!(matches_displayed_hash(expected, bytes));
    }
    assert!(!matches_displayed_hash(cases[1].1, cases[2].0));
    assert!(!matches_displayed_hash(cases[2].1, cases[3].0));
    assert!(!matches_displayed_hash(cases[4].1, cases[0].0));

    let max = vec![b'x'; 5 * 1024 * 1024];
    let expected = "sha256:dba67a476fa78973aabb087f214a1010f3bebca053674e0af50dfe5a582112be";
    let protocol_header = "sha-256=:26Z6R2+niXOquwh/IUoQEPO+vKBTZ04K9Q3+WlghEr4=:";
    assert!(notryx_protocol::verify_content_digest(protocol_header, &max).is_ok());
    assert!(matches_displayed_hash(expected, &max));
}

#[test]
fn displayed_hash_rejects_noncanonical_or_incorrect_values() {
    let valid = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert!(matches_displayed_hash(valid, b""));
    for bad in [
        "",
        "sha256:",
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8550",
        "sha256:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
        "SHA256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "sha256:g3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "sha256:é3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
        " sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        assert!(!matches_displayed_hash(bad, b""), "accepted {bad:?}");
    }
    assert!(!matches_displayed_hash(valid, b"nonempty"));
}
