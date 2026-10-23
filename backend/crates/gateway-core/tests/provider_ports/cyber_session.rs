//! Cyber 不透明语义摘要的 Key 隔离、查找顺序与有界合同

use gateway_core::{
    policy::ClientApiKeyId,
    provider_ports::{CyberSessionRequest, MAX_TRANSCRIPT_LOOKUPS},
};

#[test]
fn opaque_session_digests_are_key_scoped_and_explicit_identity_excludes_transcript() {
    let key = ClientApiKeyId::new("key-a").unwrap();
    let other = ClientApiKeyId::new("key-b").unwrap();
    let request = CyberSessionRequest::from_digests(Some([1; 32]), vec![[2; 32]]);
    let marker = request.refusal_key(&key).unwrap();
    assert_eq!(request.lookup_keys(&key), vec![marker.clone()]);
    assert!(!request.lookup_keys(&other).contains(&marker));
    assert!(
        !CyberSessionRequest::from_digests(Some([2; 32]), Vec::new())
            .lookup_keys(&key)
            .contains(&marker)
    );
    assert_eq!(format!("{marker:?}"), "CyberSessionKey([OPAQUE])");
}

#[test]
fn transcript_retains_latest_bounded_digests_and_marks_last_prefix() {
    let key = ClientApiKeyId::new("key-a").unwrap();
    let prefixes = (0..300u16)
        .map(|n| {
            let mut digest = [0; 32];
            digest[..2].copy_from_slice(&n.to_be_bytes());
            digest
        })
        .collect::<Vec<_>>();
    let first = CyberSessionRequest::from_digests(None, vec![prefixes[0]])
        .refusal_key(&key)
        .unwrap();
    let last = CyberSessionRequest::from_digests(None, vec![prefixes[299]])
        .refusal_key(&key)
        .unwrap();
    let bounded = CyberSessionRequest::from_digests(None, prefixes);
    assert_eq!(bounded.lookup_keys(&key).len(), MAX_TRANSCRIPT_LOOKUPS);
    assert!(!bounded.lookup_keys(&key).contains(&first));
    assert_eq!(bounded.refusal_key(&key), Some(last));
}
