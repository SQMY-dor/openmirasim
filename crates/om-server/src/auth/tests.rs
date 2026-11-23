use super::*;
use om_core::auth::MAX_MEMBERS_PER_SPACE;
use std::time::Duration;

struct Fx {
    _dir: tempfile::TempDir,
    db: Db,
    auth: Auth,
    space: SpaceId,
    owner: Principal,
    owner_secret: String,
}

async fn setup() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("om.db")).unwrap();
    let space = db.ensure_space("default").await.unwrap();
    let auth = Auth::new(db.clone());
    let (_, issued) = auth
        .bootstrap_owner(&space, "Ada", "first start")
        .await
        .unwrap()
        .expect("first bootstrap creates the owner");
    let owner = auth.authenticate(&issued.secret).await.unwrap();
    Fx {
        _dir: dir,
        db,
        auth,
        space,
        owner,
        owner_secret: issued.secret,
    }
}

impl Fx {
    /// Adds a member with `role` (as the owner) and signs them in.
    async fn member(&self, name: &str, role: Role) -> (Principal, String) {
        let m = self.auth.add_member(&self.owner, name, role).await.unwrap();
        let t = self
            .auth
            .issue_token(&self.owner, &m.id, "laptop", None)
            .await
            .unwrap();
        (self.auth.authenticate(&t.secret).await.unwrap(), t.secret)
    }
}

fn forbidden<T: std::fmt::Debug>(r: CoreResult<T>) {
    assert!(matches!(r, Err(CoreError::Forbidden(_))), "{r:?}");
}

fn unauthenticated<T: std::fmt::Debug>(r: CoreResult<T>) {
    assert!(matches!(r, Err(CoreError::Unauthenticated(_))), "{r:?}");
}

#[test]
fn secrets_are_prefixed_random_and_hashed() {
    let a = generate_secret().unwrap();
    let b = generate_secret().unwrap();
    assert_ne!(a, b);
    assert!(a.starts_with("omt_") && a.len() == 4 + 64);
    assert!(well_formed(&a));
    assert!(!well_formed("omt_xyz"));
    assert!(!well_formed(&a.replace("omt_", "tok_")));
    let h = hash_secret(&a);
    assert_eq!(h.len(), 64);
    assert_ne!(h, a);
    assert_eq!(h, hash_secret(&a));
}

#[tokio::test]
async fn bootstrap_runs_once_and_yields_an_owner() {
    let fx = setup().await;
    assert_eq!(fx.owner.role, Role::Owner);
    assert_eq!(fx.owner.display_name, "Ada");
    assert!(
        fx.auth
            .bootstrap_owner(&fx.space, "Mallory", "again")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(fx.db.list_members(&fx.space).await.unwrap().len(), 1);
}

#[tokio::test]
async fn only_the_hash_is_stored() {
    let fx = setup().await;
    let secret = fx.owner_secret.clone();
    let leaked: i64 = fx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM device_tokens WHERE token_hash = ?1",
                [&secret],
                |r| r.get(0),
            )
            .map_err(|e| CoreError::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(leaked, 0);
}

#[tokio::test]
async fn bad_tokens_fail_without_saying_why() {
    let fx = setup().await;
    unauthenticated(fx.auth.authenticate("").await);
    unauthenticated(fx.auth.authenticate("Bearer x").await);
    unauthenticated(fx.auth.authenticate(&generate_secret().unwrap()).await);
    // Surrounding whitespace is tolerated.
    let p = fx
        .auth
        .authenticate(&format!(" {} ", fx.owner_secret))
        .await
        .unwrap();
    assert_eq!(p.member_id, fx.owner.member_id);
}

#[tokio::test]
async fn revoked_and_expired_tokens_stop_working() {
    let fx = setup().await;
    let (bob, bob_secret) = fx.member("Bob", Role::Member).await;

    // Bob revokes his own token.
    fx.auth.revoke_token(&bob, &bob.token_id).await.unwrap();
    unauthenticated(fx.auth.authenticate(&bob_secret).await);
    // Revoking again is a no-op, not an error.
    fx.auth
        .revoke_token(&fx.owner, &bob.token_id)
        .await
        .unwrap();

    let short = fx
        .auth
        .issue_token(
            &fx.owner,
            &bob.member_id,
            "ci",
            Some(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    assert!(short.token.expires_at.is_some());
    fx.auth.authenticate(&short.secret).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    unauthenticated(fx.auth.authenticate(&short.secret).await);

    assert!(matches!(
        fx.auth
            .issue_token(&fx.owner, &bob.member_id, "x", Some(Duration::ZERO))
            .await,
        Err(CoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn authenticate_records_last_use() {
    let fx = setup().await;
    let t = fx.db.get_token(&fx.owner.token_id).await.unwrap();
    assert!(t.last_used_at.is_some());
}

#[tokio::test]
async fn disabling_a_member_revokes_every_token() {
    let fx = setup().await;
    let (bob, s1) = fx.member("Bob", Role::Member).await;
    let s2 = fx
        .auth
        .issue_token(&bob, &bob.member_id, "phone", None)
        .await
        .unwrap()
        .secret;
    fx.auth
        .disable_member(&fx.owner, &bob.member_id)
        .await
        .unwrap();
    unauthenticated(fx.auth.authenticate(&s1).await);
    unauthenticated(fx.auth.authenticate(&s2).await);
    assert!(
        fx.auth
            .list_tokens(&fx.owner, &bob.member_id)
            .await
            .unwrap()
            .iter()
            .all(|t| t.revoked_at.is_some())
    );
    // A disabled member cannot get a new token either.
    forbidden(
        fx.auth
            .issue_token(&fx.owner, &bob.member_id, "again", None)
            .await,
    );
}

#[tokio::test]
async fn viewers_and_members_cannot_manage_anyone() {
    let fx = setup().await;
    let (viewer, _) = fx.member("Vic", Role::Viewer).await;
    let (member, _) = fx.member("Mia", Role::Member).await;

    for p in [&viewer, &member] {
        forbidden(fx.auth.add_member(p, "x", Role::Viewer).await);
        forbidden(
            fx.auth
                .change_role(p, &viewer.member_id, Role::Member)
                .await,
        );
        forbidden(fx.auth.audit_log(p, None, 10).await);
        // Nobody below owner can disable themselves.
        forbidden(fx.auth.disable_member(p, &p.member_id).await);
    }
    // Members cannot touch each other's tokens.
    forbidden(fx.auth.list_tokens(&member, &viewer.member_id).await);
    forbidden(fx.auth.revoke_token(&member, &viewer.token_id).await);
    forbidden(
        fx.auth
            .issue_token(&member, &viewer.member_id, "x", None)
            .await,
    );
    // But everyone manages their own tokens and can list the team.
    fx.auth
        .issue_token(&viewer, &viewer.member_id, "tablet", None)
        .await
        .unwrap();
    assert_eq!(fx.auth.list_members(&viewer).await.unwrap().len(), 3);
    assert!(fx.auth.require(&viewer, Action::ResolveApproval).is_err());
    assert!(fx.auth.require(&member, Action::ResolveApproval).is_ok());
}

#[tokio::test]
async fn admins_manage_strictly_below_themselves() {
    let fx = setup().await;
    let (admin, _) = fx.member("Ann", Role::Admin).await;
    let (admin2, _) = fx.member("Abe", Role::Admin).await;

    let m = fx
        .auth
        .add_member(&admin, "Max", Role::Member)
        .await
        .unwrap();
    fx.auth
        .change_role(&admin, &m.id, Role::Viewer)
        .await
        .unwrap();
    fx.auth.issue_token(&admin, &m.id, "x", None).await.unwrap();

    forbidden(fx.auth.add_member(&admin, "x", Role::Admin).await);
    forbidden(fx.auth.add_member(&admin, "x", Role::Owner).await);
    forbidden(fx.auth.change_role(&admin, &m.id, Role::Admin).await);
    forbidden(
        fx.auth
            .change_role(&admin, &admin2.member_id, Role::Member)
            .await,
    );
    forbidden(fx.auth.disable_member(&admin, &admin2.member_id).await);
    forbidden(fx.auth.revoke_token(&admin, &admin2.token_id).await);
    forbidden(
        fx.auth
            .change_role(&admin, &fx.owner.member_id, Role::Viewer)
            .await,
    );
    // Admins cannot promote themselves either.
    forbidden(
        fx.auth
            .change_role(&admin, &admin.member_id, Role::Owner)
            .await,
    );

    fx.auth.disable_member(&admin, &m.id).await.unwrap();
    assert!(
        !fx.auth
            .audit_log(&admin, None, 50)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn audit_logs_are_scoped_to_the_space() {
    let fx = setup().await;
    let other = fx.db.ensure_space("other").await.unwrap();
    let (_, issued) = fx
        .auth
        .bootstrap_owner(&other, "Bo", "second space")
        .await
        .unwrap()
        .expect("first owner of the other space");
    let other_owner = fx.auth.authenticate(&issued.secret).await.unwrap();
    fx.auth
        .add_member(&fx.owner, "carol", Role::Member)
        .await
        .unwrap();
    let mine = fx.auth.audit_log(&fx.owner, None, 100).await.unwrap();
    assert!(mine.iter().any(|e| e.action == "member.created"));
    let theirs = fx.auth.audit_log(&other_owner, None, 100).await.unwrap();
    assert!(
        theirs.iter().all(|e| e.action != "member.created"),
        "another space's owner sees none of this space's entries: {theirs:?}"
    );
    assert!(
        fx.auth
            .audit_log(&other_owner, Some(fx.owner.member_id.to_string()), 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_last_owner_is_protected() {
    let fx = setup().await;
    forbidden(
        fx.auth
            .change_role(&fx.owner, &fx.owner.member_id, Role::Admin)
            .await,
    );
    forbidden(fx.auth.disable_member(&fx.owner, &fx.owner.member_id).await);

    // With a second owner, the first may step down.
    let (owner2, _) = fx.member("Olga", Role::Owner).await;
    fx.auth
        .change_role(&fx.owner, &fx.owner.member_id, Role::Admin)
        .await
        .unwrap();
    // Role changes take effect on the next request: the principal is re-read per token.
    let again = fx.auth.authenticate(&fx.owner_secret).await.unwrap();
    assert_eq!(again.role, Role::Admin);
    forbidden(fx.auth.disable_member(&again, &owner2.member_id).await);
    forbidden(fx.auth.disable_member(&owner2, &owner2.member_id).await);
}

#[tokio::test]
async fn member_cap_counts_only_active_members() {
    let fx = setup().await;
    for i in 1..MAX_MEMBERS_PER_SPACE {
        fx.auth
            .add_member(&fx.owner, &format!("m{i}"), Role::Viewer)
            .await
            .unwrap();
    }
    let over = fx
        .auth
        .add_member(&fx.owner, "one too many", Role::Viewer)
        .await;
    assert!(matches!(over, Err(CoreError::LimitExceeded(_))), "{over:?}");

    let first = fx.db.list_members(&fx.space).await.unwrap()[1].clone();
    fx.auth.disable_member(&fx.owner, &first.id).await.unwrap();
    fx.auth
        .add_member(&fx.owner, "replacement", Role::Viewer)
        .await
        .unwrap();
}

#[tokio::test]
async fn other_spaces_are_invisible() {
    let fx = setup().await;
    let other = fx.db.ensure_space("other").await.unwrap();
    let (_, foreign) = fx
        .auth
        .bootstrap_owner(&other, "Eve", "x")
        .await
        .unwrap()
        .unwrap();
    let eve = fx.auth.authenticate(&foreign.secret).await.unwrap();
    assert_eq!(eve.space_id, other);

    let not_found = |r: CoreResult<_>| matches!(r, Err(CoreError::NotFound(_)));
    assert!(not_found(
        fx.auth
            .change_role(&fx.owner, &eve.member_id, Role::Viewer)
            .await
            .map(|_| ())
    ));
    assert!(not_found(
        fx.auth
            .revoke_token(&fx.owner, &eve.token_id)
            .await
            .map(|_| ())
    ));
    assert!(not_found(
        fx.auth
            .disable_member(&eve, &fx.owner.member_id)
            .await
            .map(|_| ())
    ));
    assert_eq!(fx.auth.list_members(&eve).await.unwrap().len(), 1);
}

#[tokio::test]
async fn every_change_is_audited_with_its_actor() {
    let fx = setup().await;
    let (bob, _) = fx.member("Bob", Role::Member).await;
    fx.auth
        .change_role(&fx.owner, &bob.member_id, Role::Viewer)
        .await
        .unwrap();
    fx.auth.revoke_token(&bob, &bob.token_id).await.unwrap();
    fx.auth
        .disable_member(&fx.owner, &bob.member_id)
        .await
        .unwrap();

    let log = fx
        .auth
        .audit_log(&fx.owner, Some(bob.member_id.to_string()), 50)
        .await
        .unwrap();
    let actions: Vec<(&str, Option<&MemberId>)> = log
        .iter()
        .rev()
        .map(|e| (e.action.as_str(), e.actor.as_ref()))
        .collect();
    let owner = Some(&fx.owner.member_id);
    assert_eq!(
        actions,
        vec![
            ("member.created", owner),
            ("token.issued", owner),
            ("member.role_changed", owner),
            ("token.revoked", Some(&bob.member_id)),
            ("member.disabled", owner),
        ]
    );
    // Secrets never reach the audit log.
    assert!(log.iter().all(|e| !e.detail.to_string().contains("omt_")));
}

#[tokio::test]
async fn permission_checks_use_the_actor_s_current_role() {
    let fx = setup().await;
    let (admin, _) = fx.member("adm", Role::Admin).await;
    let target = fx
        .auth
        .add_member(&fx.owner, "victim", Role::Member)
        .await
        .unwrap();
    // The owner demotes the admin after the admin authenticated.
    fx.auth
        .change_role(&fx.owner, &admin.member_id, Role::Member)
        .await
        .unwrap();
    // The stale principal still says Admin; the write re-checks and refuses.
    forbidden(fx.auth.disable_member(&admin, &target.id).await);
    forbidden(fx.auth.add_member(&admin, "x", Role::Member).await);
    forbidden(
        fx.auth
            .issue_token(&admin, &target.id, "t", None)
            .await
            .map(|t| t.token),
    );
}

#[tokio::test]
async fn token_issuance_limits() {
    let fx = setup().await;
    let me = fx.owner.member_id.clone();
    assert!(matches!(
        fx.auth
            .issue_token(&fx.owner, &me, "short", Some(Duration::from_micros(500)))
            .await,
        Err(CoreError::InvalidInput(_))
    ));
    // A short-lived token cannot mint a permanent one.
    let short = fx
        .auth
        .issue_token(&fx.owner, &me, "short", Some(Duration::from_secs(120)))
        .await
        .unwrap();
    let p = fx.auth.authenticate(&short.secret).await.unwrap();
    let child = fx.auth.issue_token(&p, &me, "child", None).await.unwrap();
    assert_eq!(child.token.expires_at, short.token.expires_at);
}
