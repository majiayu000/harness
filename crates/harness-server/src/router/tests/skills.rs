use super::*;

#[tokio::test]
async fn skill_create_and_get() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let create_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::SkillCreate {
            name: "test-skill".to_string(),
            content: "# Test Skill\nDoes things.".to_string(),
        },
    };
    let create_resp = handle_request(&state, create_req)
        .await
        .expect("expected response");
    assert!(
        create_resp.error.is_none(),
        "skill_create should succeed: {:?}",
        create_resp.error
    );
    let skill = create_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let skill_id_str = skill["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing id"))?
        .to_string();
    let skill_id = harness_core::types::SkillId::from_str(&skill_id_str);

    let get_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::SkillGet { skill_id },
    };
    let get_resp = handle_request(&state, get_req)
        .await
        .expect("expected response");
    assert!(
        get_resp.error.is_none(),
        "skill_get should succeed: {:?}",
        get_resp.error
    );
    let retrieved = get_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(retrieved["name"], serde_json::json!("test-skill"));
    Ok(())
}

#[tokio::test]
async fn skill_list_returns_created_skills() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    for name in ["alpha-skill", "beta-skill"] {
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::SkillCreate {
                name: name.to_string(),
                content: format!("# {name}\nDoes things."),
            },
        };
        let resp = handle_request(&state, req).await.expect("response");
        assert!(resp.error.is_none(), "skill_create should succeed");
    }

    let list_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::SkillList { query: None },
    };
    let list_resp = handle_request(&state, list_req)
        .await
        .expect("expected response");
    assert!(
        list_resp.error.is_none(),
        "skill_list should succeed: {:?}",
        list_resp.error
    );
    let skill_count = list_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array result"))?
        .len();
    assert_eq!(skill_count, 2, "should list both created skills");
    Ok(())
}

#[tokio::test]
async fn skill_delete_removes_skill() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let create_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::SkillCreate {
            name: "to-delete".to_string(),
            content: "# Delete Me".to_string(),
        },
    };
    let create_resp = handle_request(&state, create_req).await.expect("response");
    let skill_id_str = create_resp.result.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let skill_id = harness_core::types::SkillId::from_str(&skill_id_str);

    let delete_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::SkillDelete { skill_id },
    };
    let delete_resp = handle_request(&state, delete_req).await.expect("response");
    assert!(
        delete_resp.error.is_none(),
        "skill_delete should succeed: {:?}",
        delete_resp.error
    );
    let result = delete_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["deleted"], serde_json::json!(true));
    Ok(())
}
