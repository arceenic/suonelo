use super::*;
use serde_json::json;

#[test]
fn typed_rows_preserve_identity_collaborators_and_independent_flags() {
    let album = json!({
        "id":"000abc", "title":"Petal", "subtitle":"Ariana Grande", "version":null,
        "artist":{"id":713899,"name":"Ariana Grande"}, "artists":[
            {"id":17,"name":"Cynthia Erivo","roles":["main-artist"]},
            {"id":18,"name":"Producer","roles":["producer"]}
        ], "genre":{"id":117}, "parental_warning":true, "hires":true,
        "maximum_bit_depth":24, "maximum_sampling_rate":48, "release_date_original":"2026-01-01"
    });
    let track = json!({
        "id":9007199254740993_u64, "title":"Petal", "album":album,
        "parental_warning":false, "hires_streamable":true,
        "performer":{"id":17,"name":"Cynthia Erivo"}
    });
    let track = row(Kind::Track, &track, None).unwrap();
    let album = track.parent.unwrap();
    assert_eq!(track.key.id, "9007199254740993");
    assert_eq!(album.key.id, "000abc");
    assert_eq!(track.explicit, Some(false));
    assert_eq!(album.explicit, Some(true));
    assert_eq!(track.hires, None);
    assert_eq!(album.edition, None);
    assert_eq!(album.credits.len(), 2);
    assert_eq!(album.primary_credit.as_deref(), Some("713899"));
}

#[tokio::test]
#[ignore = "Requires explicitly selected, authorized Qobuz portable vault and live connectivity"]
async fn live_catalog_probe() {
    let vault_path =
        std::env::var_os("SUONELO_SEARCH_VAULT").expect("Choose the approved QA vault");
    let vault = crate::vault::Vault::new(vault_path.into());
    let account = vault
        .credentials()
        .expect("Could not open the QA vault with Windows data protection");
    let client = QobuzClient::new().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(45), async {
        match client.request("user/get", Vec::new, account).await {
            Ok(body) => println!("account verified; numeric ID present={}", body["id"].as_u64().or_else(|| body["user"]["id"].as_u64()).is_some()),
            Err(failure) => println!("account probe returned {} ({:?})", failure.error.code, failure.status),
        }
        for (offset, limit, parent) in [(0, 100, None), (10, 10, None), (0, 100, Some(117)), (0, 100, Some(92)), (0, 100, Some(999999))] {
            let mut params = vec![("offset", offset.to_string()), ("limit", limit.to_string())];
            if let Some(parent) = parent { params.push(("parent_id", parent.to_string())); }
            match client.request("genre/list", || params.clone(), account).await {
                Ok(value) => {
                    let genres = &value["genres"];
                    let rows = genres["items"].as_array().into_iter().flatten().map(|row| {
                        json!({"id":row["id"],"name":row["name"],"slug":row["slug"],"path":row["path"],"parent_id":row["parent_id"],"children":row["children"].as_array().map(Vec::len)})
                    }).collect::<Vec<_>>();
                    println!("taxonomy offset={offset} limit={limit} parent={parent:?} total={} items={}", genres["total"], json!(rows));
                }
                Err(failure) => println!("taxonomy request returned {} ({:?})", failure.error.code, failure.status),
            }
        }
        for query in ["petal ariana", "petal ari", "beyonce"] {
            let groups = client.search_catalog(query, account).await.map_err(|e| e.code).unwrap();
            for (kind, page) in groups {
                let page = page.map_err(|e| e.code).unwrap();
                println!("{query} {kind:?}: {}/ {:?}, first {:?}", page.length, page.total, page.rows.first().map(|row| (&row.key.id, &row.title)));
            }
        }
    }).await;
    assert!(result.is_ok(), "Live Qobuz catalog probe timed out");
}

#[tokio::test]
#[ignore = "Read-only recursive taxonomy verification with an authorized Qobuz portable vault"]
async fn capture_catalog_taxonomy() {
    let vault =
        crate::vault::Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
    let account = vault
        .credentials()
        .expect("Could not open the selected vault");
    let client = QobuzClient::new().unwrap();
    let mut pending = std::collections::VecDeque::<Option<u64>>::from([None]);
    let mut taxonomy = std::collections::BTreeMap::new();
    let mut requests = 0;
    while let Some(parent) = pending.pop_front() {
        let mut offset = 0;
        loop {
            requests += 1;
            assert!(requests <= 512, "Unexpected taxonomy size");
            let mut params = vec![("offset", offset.to_string()), ("limit", "20".to_string())];
            if let Some(id) = parent {
                params.push(("parent_id", id.to_string()));
            }
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(12),
                client.metadata("genre/list", params, account),
            )
            .await
            .expect("Taxonomy deadline exceeded")
            .unwrap_or_else(|err| panic!("taxonomy failed: {}", err.code));
            let group = &response["genres"];
            let items = group["items"].as_array().expect("Missing genre group");
            let total = group["total"].as_u64().expect("Missing genre total") as usize;
            for item in items {
                let id = item["id"].as_u64().expect("Missing canonical genre ID");
                let label = item["name"].as_str().expect("Missing localized label");
                let slug = item["slug"].as_str().expect("Missing genre slug");
                let path = item["path"]
                    .as_array()
                    .expect("Missing canonical path")
                    .iter()
                    .map(|x| x.as_u64().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(path.first().copied(), Some(id));
                assert_eq!(path.get(1).copied(), parent);
                assert!(taxonomy.insert(id, json!({"id":id,"label":label,"slug":slug,"parent":parent,"path":path})).is_none());
                pending.push_back(Some(id));
            }
            offset += items.len();
            assert!(offset <= total);
            if offset == total {
                break;
            }
            assert!(!items.is_empty(), "Non-advancing genre pagination");
        }
    }
    println!(
        "Verified {} catalog nodes over {requests} pages, {} roots",
        taxonomy.len(),
        taxonomy.values().filter(|x| x["parent"].is_null()).count()
    );
    for parent in [112, 91, 116, 123, 167, 0] {
        let params = vec![
            ("offset", "0".into()),
            ("limit", "1".into()),
            ("parent_id", parent.to_string()),
        ];
        match client
            .request("genre/list", || params.clone(), account)
            .await
        {
            Ok(v) => println!(
                "additional genre root {parent}: total={}",
                v["genres"]["total"]
            ),
            Err(err) => println!(
                "additional genre root {parent}: {} ({:?})",
                err.error.code, err.status
            ),
        }
    }
    let asset = json!({"apiVersion":"0.2","verifiedOn":"2026-11-07","genres":taxonomy.into_values().collect::<Vec<_>>()});
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(std::env::var_os("SUONELO_SEARCH_TAXONOMY_CAPTURE").unwrap())
        .unwrap();
    serde_json::to_writer_pretty(&mut file, &asset).unwrap();
}

#[tokio::test]
#[ignore = "Read-only taxonomy scope comparison using authorized application access"]
async fn taxonomy_scope_probe() {
    let vault =
        crate::vault::Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
    let account = vault
        .credentials()
        .expect("Could not open the selected vault");
    let client = QobuzClient::new().unwrap();
    for (name, params) in [
        ("baseline", vec![("limit", "100")]),
        ("country", vec![("limit", "100"), ("country", "GB")]),
        (
            "country_code",
            vec![("limit", "100"), ("country_code", "GB")],
        ),
        ("territory", vec![("limit", "100"), ("territory", "GB")]),
        ("lang", vec![("limit", "100"), ("lang", "en")]),
        ("pop rock", vec![("limit", "100"), ("parent_id", "112")]),
        ("reggae", vec![("limit", "100"), ("parent_id", "123")]),
    ] {
        for authenticate in [false, true] {
            let mut request = client
                .api
                .get(format!("{}genre/list", super::super::API))
                .header("X-App-Id", &account.app_id)
                .header("Accept-Language", "en-GB")
                .header("Origin", super::super::PLAYER)
                .query(&params);
            if authenticate {
                request = request.query(&[("user_auth_token", &account.token)]);
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let body = response
                        .bytes()
                        .await
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .unwrap_or(Value::Null);
                    let genre_ids = body["genres"]["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|item| (&item["id"], &item["slug"], &item["path"]))
                        .collect::<Vec<_>>();
                    println!(
                        "{name} authenticated={authenticate} HTTP {status}: {}",
                        json!(genre_ids)
                    );
                }
                Err(_) => println!("{name} authenticated={authenticate} transport failed"),
            }
        }
    }
    for id in [112, 91, 116, 123, 167] {
        let params = vec![("genre_id", id.to_string())];
        match client
            .request("genre/get", || params.clone(), account)
            .await
        {
            Ok(row) => println!(
                "genre/get {id}: name={} slug={} path={} keys={:?}",
                row["name"],
                row["slug"],
                row["path"],
                row.as_object().map(|obj| obj.keys().collect::<Vec<_>>())
            ),
            Err(err) => println!("genre/get {id}: {} ({:?})", err.error.code, err.status),
        }
    }
    for (field, slug) in [
        ("slug", "anime"),
        ("genre_id", "anime"),
        ("slug", "musique-de-films"),
        ("genre_id", "musique-de-films"),
    ] {
        let params = vec![(field, slug.to_owned())];
        match client
            .request("genre/get", || params.clone(), account)
            .await
        {
            Ok(row) => println!(
                "genre/get by {field}={slug}: id={} slug={}",
                row["id"], row["slug"]
            ),
            Err(err) => println!(
                "genre/get by {field}={slug}: {} ({:?})",
                err.error.code, err.status
            ),
        }
    }
}

#[tokio::test]
#[ignore = "Resolves storefront category IDs omitted by the acceptance account's genre/list tree"]
async fn resolve_storefront_genre_ids() {
    let vault =
        crate::vault::Vault::new(std::env::var_os("SUONELO_SEARCH_VAULT").unwrap().into());
    let account = vault
        .credentials()
        .expect("Could not open the selected vault");
    let client = QobuzClient::new().unwrap();
    let capture: Value = serde_json::from_reader(
        std::fs::File::open(std::env::var_os("SUONELO_SEARCH_TAXONOMY_CAPTURE").unwrap())
            .unwrap(),
    )
    .unwrap();
    let ids = capture["genres"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_u64().unwrap())
        .collect::<std::collections::HashSet<_>>();
    let mut found = Vec::new();
    let lower = *ids.iter().min().unwrap();
    let upper = *ids.iter().max().unwrap();
    assert!(
        upper - lower < 256,
        "Review a larger canonical ID range before probing"
    );
    let range = if std::env::var_os("SUONELO_SEARCH_GENRE_NEW_IDS").is_some() {
        upper + 1..=upper + 20
    } else {
        lower..=upper
    };
    for id in range.filter(|id| !ids.contains(id)) {
        let params = vec![("genre_id", id.to_string())];
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.request("genre/get", || params.clone(), account),
        )
        .await
        .expect("Genre identity request exceeded deadline");
        match response {
            Ok(row) => {
                assert_eq!(row["id"].as_u64(), Some(id));
                found.push(json!({"id":row["id"],"label":row["name"],"slug":row["slug"],"path":row["path"]}));
            }
            Err(error) if error.status == Some(reqwest::StatusCode::NOT_FOUND) => (),
            Err(error) => panic!(
                "Genre lookup failed for {id}: {} {:?}",
                error.error.code, error.status
            ),
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(std::env::var_os("SUONELO_SEARCH_GENRE_IDENTITIES").unwrap())
        .unwrap();
    println!("Verified {} further genre identities", found.len());
    serde_json::to_writer_pretty(&mut file, &found).unwrap();
}
