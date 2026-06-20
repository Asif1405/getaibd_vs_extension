use mcp_universal::tools::approval::ApprovalGate;

#[tokio::test]
async fn approval_granted_returns_true() {
    let gate = ApprovalGate::new();
    let gate_clone = gate.clone();

    let handle = tokio::spawn(async move { gate_clone.request("req_1".into()).await });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sent = gate.respond("req_1", true).await;
    assert!(sent);
    assert!(handle.await.unwrap());
}

#[tokio::test]
async fn approval_denied_returns_false() {
    let gate = ApprovalGate::new();
    let gate_clone = gate.clone();

    let handle = tokio::spawn(async move { gate_clone.request("req_2".into()).await });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let sent = gate.respond("req_2", false).await;
    assert!(sent);
    assert!(!handle.await.unwrap());
}

#[tokio::test]
async fn respond_unknown_id_returns_false() {
    let gate = ApprovalGate::new();
    let sent = gate.respond("nonexistent", true).await;
    assert!(!sent);
}

#[tokio::test]
async fn multiple_pending_requests() {
    let gate = ApprovalGate::new();
    let g1 = gate.clone();
    let g2 = gate.clone();

    let h1 = tokio::spawn(async move { g1.request("a".into()).await });
    let h2 = tokio::spawn(async move { g2.request("b".into()).await });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    gate.respond("a", true).await;
    gate.respond("b", false).await;

    assert!(h1.await.unwrap());
    assert!(!h2.await.unwrap());
}
