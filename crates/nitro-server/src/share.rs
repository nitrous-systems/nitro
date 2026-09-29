//! Cross-client Surface sharing (#3904): the export/import table.
//!
//! A connection **exports** one of its Surface nodes and gets a bearer
//! [`ShareToken`]; another connection of the same uid **imports** the
//! token under an id of its own and may then `PresentSurface` into the
//! node. This table is the single source of truth for which imports are
//! live. It never sends anything: every method returns the imports it
//! revoked, and the server tells the importers. The precise contract is
//! in `docs/wire.md` under "Surface sharing".

use std::collections::HashMap;

use nitro_scene::{NodeKey, NodeKind, Scene};
use nitro_wire::types::{NodeId, ShareToken};

/// Most imports (live or dead) one client may hold.
pub const MAX_IMPORTS_PER_CLIENT: usize = 256;

/// An import that just died: tell `importer` that `id` is revoked, and
/// drop whatever it had queued on `node`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Revoked {
    /// The importing connection.
    pub importer: u64,
    /// Its id for the import.
    pub id: NodeId,
    /// The node the import pointed at.
    pub node: NodeKey,
}

/// One exported node.
#[derive(Debug, Clone, Copy)]
struct Export {
    /// The owning connection.
    owner: u64,
    /// The owner's peer uid, if the kernel told us.
    uid: Option<u32>,
    /// The node.
    node: NodeKey,
    /// The live import, if any: `(importer, its id)`.
    import: Option<(u64, NodeId)>,
}

/// Why an `ImportSurface` is a protocol error rather than a dead import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportError {
    /// The importer exported this token itself.
    OwnToken,
}

/// The result of a successful `ImportSurface`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// The node, or `None` when the import is dead from the start (an
    /// unknown, stale or rotated token, or a uid mismatch).
    pub node: Option<NodeKey>,
    /// The import this one displaced.
    pub displaced: Option<Revoked>,
}

/// Every live export.
#[derive(Debug, Default)]
pub struct Shares {
    exports: HashMap<ShareToken, Export>,
    by_node: HashMap<NodeKey, ShareToken>,
}

fn revoke(e: &Export) -> Option<Revoked> {
    e.import.map(|(importer, id)| Revoked {
        importer,
        id,
        node: e.node,
    })
}

impl Shares {
    /// Export `node` (owned by `owner`, peer uid `uid`) under the fresh
    /// `token`. A previous token for the node stops working, and its live
    /// import, if any, is returned revoked.
    pub fn export(
        &mut self,
        owner: u64,
        uid: Option<u32>,
        node: NodeKey,
        token: ShareToken,
    ) -> Option<Revoked> {
        let old = self
            .by_node
            .insert(node, token)
            .and_then(|t| self.exports.remove(&t))
            .and_then(|e| revoke(&e));
        self.exports.insert(
            token,
            Export {
                owner,
                uid,
                node,
                import: None,
            },
        );
        old
    }

    /// Redeem `token` for `importer` (peer uid `uid`) as `id`. The newest
    /// importer wins; the one it displaced is returned.
    ///
    /// # Errors
    /// [`ImportError::OwnToken`] when the importer is the exporter.
    pub fn import(
        &mut self,
        token: ShareToken,
        importer: u64,
        uid: Option<u32>,
        id: NodeId,
    ) -> Result<Imported, ImportError> {
        let Some(e) = self.exports.get_mut(&token) else {
            return Ok(Imported {
                node: None,
                displaced: None,
            });
        };
        if e.owner == importer {
            return Err(ImportError::OwnToken);
        }
        if uid.is_none() || e.uid != uid {
            return Ok(Imported {
                node: None,
                displaced: None,
            });
        }
        let displaced = revoke(e);
        e.import = Some((importer, id));
        Ok(Imported {
            node: Some(e.node),
            displaced,
        })
    }

    /// The node `importer`'s `id` presents into, if that import is live.
    #[must_use]
    pub fn live_import(&self, importer: u64, id: NodeId) -> Option<NodeKey> {
        self.exports
            .values()
            .find(|e| e.import == Some((importer, id)))
            .map(|e| e.node)
    }

    /// The importer dropped `id` (`DestroyNode`). Returns the node it
    /// pointed at, if the import was still live.
    pub fn drop_import(&mut self, importer: u64, id: NodeId) -> Option<NodeKey> {
        let e = self
            .exports
            .values_mut()
            .find(|e| e.import == Some((importer, id)))?;
        e.import = None;
        Some(e.node)
    }

    /// A connection went away: its exports die (their imports are
    /// returned revoked) and its imports are forgotten — the token stays
    /// valid for a restarted importer.
    pub fn forget_client(&mut self, token: u64) -> Vec<Revoked> {
        let mut out = Vec::new();
        self.exports.retain(|_, e| {
            if e.owner == token {
                out.extend(revoke(e));
                return false;
            }
            if e.import.is_some_and(|(t, _)| t == token) {
                e.import = None;
            }
            true
        });
        self.by_node.retain(|_, t| self.exports.contains_key(t));
        out
    }

    /// Drop every export whose node is gone (or no longer a Surface),
    /// returning their live imports, revoked. Node keys are generational,
    /// so a recycled slot never aliases an old export.
    pub fn sweep(&mut self, scene: &Scene) -> Vec<Revoked> {
        if self.exports.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.exports.retain(|_, e| {
            let alive = scene
                .node(e.node)
                .is_ok_and(|n| n.kind() == NodeKind::Surface);
            if !alive {
                out.extend(revoke(e));
            }
            alive
        });
        self.by_node.retain(|_, t| self.exports.contains_key(t));
        out
    }

    /// Whether nothing is exported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exports.is_empty()
    }
}

/// Mint a fresh token from the kernel's CSPRNG.
///
/// # Errors
/// The `getrandom` failure, which on Linux ≥ 3.17 means something is
/// badly wrong.
pub fn mint() -> std::io::Result<ShareToken> {
    let mut buf = [0u8; 16];
    let mut filled = 0;
    while filled < buf.len() {
        let n = rustix::rand::getrandom(&mut buf[filled..], rustix::rand::GetRandomFlags::empty())?;
        filled += n;
    }
    Ok(ShareToken(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_core::{Point, Size};
    use nitro_scene::{ClientId, Layer};

    const C: ClientId = ClientId(1);
    const A: u64 = 10;
    const B: u64 = 20;
    const D: u64 = 30;
    const UID: Option<u32> = Some(1000);

    fn world() -> (Scene, NodeKey) {
        let mut s = Scene::new();
        s.add_output(
            nitro_scene::OutputId(0),
            nitro_core::IRect::new(0, 0, 100, 100),
            1.0,
        );
        let win = s.create_window(C, "w", Size::new(50.0, 50.0), Layer::Normal);
        s.place_window(win, Some(nitro_scene::OutputId(0)), Point::ZERO)
            .unwrap();
        let root = s.window_info(win).unwrap().root();
        let n = s.create_node(C, NodeKind::Surface, root, None).unwrap();
        (s, n)
    }

    fn tok(b: u8) -> ShareToken {
        ShareToken([b; 16])
    }

    #[test]
    fn a_uid_mismatch_or_unknown_token_is_a_dead_import() {
        let (_, n) = world();
        let mut s = Shares::default();
        assert_eq!(s.export(A, UID, n, tok(1)), None);
        let dead = s.import(tok(1), B, Some(1001), NodeId(5)).unwrap();
        assert_eq!(dead.node, None);
        assert_eq!(s.import(tok(1), B, None, NodeId(5)).unwrap().node, None);
        assert_eq!(s.import(tok(9), B, UID, NodeId(5)).unwrap().node, None);
        assert_eq!(s.live_import(B, NodeId(5)), None);
        assert_eq!(
            s.import(tok(1), A, UID, NodeId(5)),
            Err(ImportError::OwnToken)
        );
        let live = s.import(tok(1), B, UID, NodeId(5)).unwrap();
        assert_eq!(live.node, Some(n));
        assert_eq!(s.live_import(B, NodeId(5)), Some(n));
    }

    #[test]
    fn re_export_rotates_and_the_newest_importer_wins() {
        let (_, n) = world();
        let mut s = Shares::default();
        s.export(A, UID, n, tok(1));
        s.import(tok(1), B, UID, NodeId(5)).unwrap();
        let second = s.import(tok(1), D, UID, NodeId(6)).unwrap();
        assert_eq!(
            second.displaced,
            Some(Revoked {
                importer: B,
                id: NodeId(5),
                node: n
            })
        );
        assert_eq!(s.live_import(B, NodeId(5)), None);
        let old = s.export(A, UID, n, tok(2));
        assert_eq!(
            old,
            Some(Revoked {
                importer: D,
                id: NodeId(6),
                node: n
            })
        );
        assert_eq!(s.import(tok(1), B, UID, NodeId(7)).unwrap().node, None);
        assert_eq!(s.import(tok(2), B, UID, NodeId(7)).unwrap().node, Some(n));
        assert_eq!(s.drop_import(B, NodeId(7)), Some(n));
        assert_eq!(s.drop_import(B, NodeId(7)), None);
    }

    #[test]
    fn a_destroyed_node_or_owner_revokes_and_an_importer_leaving_does_not() {
        let (mut scene, n) = world();
        let mut s = Shares::default();
        s.export(A, UID, n, tok(1));
        s.import(tok(1), B, UID, NodeId(5)).unwrap();
        assert!(s.sweep(&scene).is_empty());
        // The importer goes: the token survives for a restarted one.
        assert!(s.forget_client(B).is_empty());
        assert_eq!(s.import(tok(1), D, UID, NodeId(6)).unwrap().node, Some(n));
        scene.destroy_node(C, n).unwrap();
        assert_eq!(
            s.sweep(&scene),
            vec![Revoked {
                importer: D,
                id: NodeId(6),
                node: n
            }]
        );
        assert!(s.is_empty());

        let (_, n2) = world();
        s.export(A, UID, n2, tok(3));
        s.import(tok(3), B, UID, NodeId(1)).unwrap();
        assert_eq!(s.forget_client(A).len(), 1);
        assert!(s.is_empty());
    }

    #[test]
    fn minted_tokens_differ() {
        assert_ne!(mint().unwrap(), mint().unwrap());
    }
}
