//! META-12: манифесты поисковых индексов на PostgreSQL.

mod common;

use std::sync::Arc;

use time::OffsetDateTime;
use tp_contracts::Error;
use tp_contracts::search::{
    IndexManifest, IndexManifestStore, IndexSegment, ManifestUpdate, RebuildReason, RebuildRequest,
};

fn seg(n: u64) -> IndexSegment {
    IndexSegment {
        path: format!("idx/t/a/{n}.seg"),
        size: n * 100,
        docs: n,
    }
}

fn update(schema: u32, segs: &[u64], rebuilt: bool) -> ManifestUpdate {
    ManifestUpdate {
        schema,
        segments: segs.iter().copied().map(seg).collect(),
        rebuilt,
    }
}

fn request(reason: RebuildReason) -> RebuildRequest {
    RebuildRequest {
        requested_at: OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("ts"),
        reason,
    }
}

#[tokio::test]
async fn cas_versions() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    assert_eq!(
        meta.get_manifest(scope).await.expect("get"),
        IndexManifest::default()
    );
    assert_eq!(
        meta.put_manifest(scope, 1, update(1, &[1], false)).await,
        Err(Error::Conflict)
    );
    let v1 = meta
        .put_manifest(scope, 0, update(1, &[1], false))
        .await
        .expect("first");
    assert_eq!(v1, 1);
    assert_eq!(
        meta.put_manifest(scope, 0, update(1, &[2], false)).await,
        Err(Error::Conflict)
    );
    let v2 = meta
        .put_manifest(scope, v1, update(1, &[1, 2], false))
        .await
        .expect("second");
    assert_eq!(v2, 2);
    let m = meta.get_manifest(scope).await.expect("get");
    assert_eq!(
        m,
        IndexManifest {
            version: 2,
            schema: 1,
            segments: vec![seg(1), seg(2)],
            rebuild: None,
        }
    );
    assert_eq!((m.size(), m.docs()), (300, 3));
    assert_eq!(
        meta.put_manifest(scope, v1, update(1, &[3], false)).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.put_manifest(scope, u64::MAX, update(1, &[3], false))
            .await,
        Err(Error::Conflict)
    );

    // Другая организация манифест не видит.
    let other = common::account(&meta).await;
    let foreign = tp_contracts::meta::AccountScope {
        tenant: other.tenant,
        account: scope.account,
    };
    assert_eq!(
        meta.get_manifest(foreign).await.expect("get"),
        IndexManifest::default()
    );
}

#[tokio::test]
async fn concurrent_put_one_wins() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let meta = Arc::new(meta);
    let scope = common::account(&meta).await;
    for expected in [0, 1] {
        let tasks: Vec<_> = (0..16u64)
            .map(|i| {
                let meta = Arc::clone(&meta);
                tokio::spawn(async move {
                    (
                        i,
                        meta.put_manifest(scope, expected, update(1, &[100 + i], false))
                            .await,
                    )
                })
            })
            .collect();
        let mut winners = Vec::new();
        for t in tasks {
            match t.await.expect("join") {
                (i, Ok(v)) => winners.push((i, v)),
                (_, Err(Error::Conflict)) => {}
                (_, Err(e)) => panic!("неожиданная ошибка: {e:?}"),
            }
        }
        assert_eq!(winners.len(), 1, "побеждает одна запись: {winners:?}");
        let (i, v) = winners[0];
        assert_eq!(v, expected + 1);
        let m = meta.get_manifest(scope).await.expect("get");
        assert_eq!(m.version, v);
        assert_eq!(m.segments, vec![seg(100 + i)]);
    }
}

#[tokio::test]
async fn rebuild_bumps_version_and_is_cleared() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let v1 = meta
        .put_manifest(scope, 0, update(1, &[1], false))
        .await
        .expect("put");
    // Идёт перестройка от версии v1; приходит запрос — переключение получит Conflict.
    let v2 = meta
        .request_rebuild(scope, request(RebuildReason::Corrupt))
        .await
        .expect("rebuild");
    assert_eq!(v2, v1 + 1);
    assert_eq!(
        meta.put_manifest(scope, v1, update(2, &[5], true)).await,
        Err(Error::Conflict)
    );
    let m = meta.get_manifest(scope).await.expect("get");
    assert_eq!(m.rebuild, Some(request(RebuildReason::Corrupt)));
    assert_eq!(
        m.segments,
        vec![seg(1)],
        "прежний индекс обслуживает запросы"
    );
    // Повторный запрос заменяет прежний.
    let v3 = meta
        .request_rebuild(scope, request(RebuildReason::Admin))
        .await
        .expect("again");
    // Обычная запись не снимает запрос.
    let v4 = meta
        .put_manifest(scope, v3, update(1, &[1, 2], false))
        .await
        .expect("incremental");
    assert_eq!(
        meta.get_manifest(scope).await.expect("get").rebuild,
        Some(request(RebuildReason::Admin))
    );
    let v5 = meta
        .put_manifest(scope, v4, update(2, &[7], true))
        .await
        .expect("rebuilt");
    assert_eq!(
        meta.get_manifest(scope).await.expect("get"),
        IndexManifest {
            version: v5,
            schema: 2,
            segments: vec![seg(7)],
            rebuild: None,
        }
    );

    // Запрос для учётной записи без индекса создаёт манифест без сегментов.
    let fresh = common::account(&meta).await;
    assert_eq!(
        meta.request_rebuild(fresh, request(RebuildReason::Restore))
            .await
            .expect("rebuild"),
        1
    );
    let m = meta.get_manifest(fresh).await.expect("get");
    assert!(m.segments.is_empty());
    assert_eq!(m.rebuild, Some(request(RebuildReason::Restore)));
    assert_eq!(
        meta.put_manifest(fresh, 0, update(2, &[1], true)).await,
        Err(Error::Conflict)
    );
}

#[tokio::test]
async fn pending_rebuilds_across_tenants() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let requested = common::account(meta).await;
    let old_schema = common::account(meta).await;
    let current = common::account(meta).await;
    // Учётная запись без индекса в выборку не попадает.
    let _without_index = common::account(meta).await;
    meta.put_manifest(requested, 0, update(2, &[1], false))
        .await
        .expect("put");
    meta.request_rebuild(requested, request(RebuildReason::Admin))
        .await
        .expect("rebuild");
    meta.put_manifest(old_schema, 0, update(1, &[1], false))
        .await
        .expect("put");
    meta.put_manifest(current, 0, update(2, &[1], false))
        .await
        .expect("put");

    let got = meta.pending_rebuilds(2, 10).await.expect("pending");
    assert_eq!(got, vec![requested, old_schema]);
    assert_eq!(
        meta.pending_rebuilds(2, 1).await.expect("limit"),
        vec![requested]
    );
    assert_eq!(
        meta.pending_rebuilds(1, 10).await.expect("schema 1"),
        vec![requested]
    );
    // Перестройка выполнена — запрос снят.
    let v = meta.get_manifest(requested).await.expect("get").version;
    meta.put_manifest(requested, v, update(2, &[2], true))
        .await
        .expect("rebuilt");
    assert_eq!(
        meta.pending_rebuilds(2, 10).await.expect("pending"),
        vec![old_schema]
    );

    db.drop_db().await;
}
