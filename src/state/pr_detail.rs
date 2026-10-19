//! One open PR's session: the load orchestration and everything the detail
//! screen draws from.
//!
//! Loading is a small state machine driven entirely by [`Event`]s: metadata
//! first (so the head SHA is known before any file's patch is anchored to it),
//! then the files page by page. Events carry the [`SessionId`] they belong to,
//! so a reply for a PR the user has left is dropped rather than folded into the
//! session now on screen (the stale-event rule).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use crate::diff::row_model::{build, RowModel};
use crate::github::error::GhError;
use crate::github::files::LoadedFile;
use crate::github::metadata::PrMetadata;
use crate::model::RepoRef;
use crate::persist::session::{self, DebouncedWriter, SessionFile};
use crate::runtime::{Command, Event, SessionId};
use crate::state::filters::FilterState;
use crate::state::viewed::ViewedSet;

/// How far a session has got through loading its PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadPhase {
    /// Waiting for the metadata.
    Header,
    /// Metadata is in; the files are still arriving.
    Files,
    /// Everything the detail screen needs is loaded.
    Ready,
    /// Loading stopped on an error.
    Failed,
}

/// One open PR, its loaded data, and its loading progress.
///
/// Later phases attach their own state here (`viewed`, `filters`, `threads`,
/// `pending`, `reviews`) rather than in the app.
pub struct PrSession {
    /// The id echoed by every PR-scoped event this session owns.
    pub id: SessionId,
    /// The GitHub host the PR lives on.
    pub host: String,
    /// The repository the PR lives in.
    pub repo: RepoRef,
    /// The PR number within the repository.
    pub number: u64,
    /// The PR's metadata, once loaded.
    pub metadata: Option<PrMetadata>,
    /// The head SHA the displayed diff came from (spec §7.3).
    pub diff_head_sha: Option<String>,
    /// The PR's files, in API order, with their patches parsed.
    pub files: Vec<LoadedFile>,
    /// The flat row list the diff view virtualizes.
    pub row_model: RowModel,
    /// How far loading has got.
    pub phase: LoadPhase,
    /// The failure that stopped loading, if any.
    pub error: Option<GhError>,
    /// Whether the head moved since the diff on screen was loaded.
    pub head_moved: bool,
    /// Which files the reviewer has marked viewed, keyed by blob SHA.
    pub viewed: ViewedSet,
    /// What the file tree is filtering the file list by.
    pub filters: FilterState,
    /// The file-tree directories the reviewer has collapsed, by path.
    pub collapsed_dirs: BTreeSet<String>,
    /// The last file selected in the tree, restored from the session file.
    pub last_opened_file: Option<String>,
    /// The config root the session file lives under.
    root: PathBuf,
    /// Coalesces viewed/collapse changes into debounced writes.
    writer: DebouncedWriter,
}

impl PrSession {
    /// Open a PR: build the session and the commands that start its load.
    ///
    /// Only the metadata is requested here; the files are requested from
    /// [`PrSession::on_event`] when the metadata arrives, so the diff on screen
    /// and `diff_head_sha` are always the same commit.
    pub fn open(
        id: SessionId,
        host: String,
        repo: RepoRef,
        number: u64,
    ) -> (PrSession, Vec<Command>) {
        let root = crate::persist::config_root();
        let (viewed, collapsed_dirs, last_opened_file) =
            match session::load(&root, &host, &repo, number) {
                Ok(Some(file)) => (file.viewed, file.collapsed_dirs, file.last_opened_file),
                Ok(None) => (ViewedSet::default(), BTreeSet::new(), None),
                Err(error) => {
                    tracing::warn!(%error, "could not read the PR session file");
                    (ViewedSet::default(), BTreeSet::new(), None)
                }
            };

        let session = PrSession {
            id,
            host,
            repo: repo.clone(),
            number,
            metadata: None,
            diff_head_sha: None,
            files: Vec::new(),
            row_model: build(&[]),
            phase: LoadPhase::Header,
            error: None,
            head_moved: false,
            viewed,
            filters: FilterState::default(),
            collapsed_dirs,
            last_opened_file,
            root,
            writer: DebouncedWriter::default(),
        };

        let commands = vec![Command::LoadPrMetadata {
            session: id,
            repo,
            number,
        }];
        (session, commands)
    }

    /// Fold one worker event in and return the commands it triggers.
    ///
    /// An event whose session is not this one is ignored entirely — that is
    /// what keeps a slow reply for a closed PR from corrupting this one.
    pub fn on_event(&mut self, event: &Event) -> Vec<Command> {
        match event {
            Event::PrMetadataLoaded { session, meta } if *session == self.id => {
                // The head is only "moved" once we have a diff to compare it
                // against; the first load has nothing to invalidate.
                if let Some(shown) = &self.diff_head_sha {
                    if shown != &meta.head_sha {
                        self.head_moved = true;
                    }
                }
                self.metadata = Some(meta.clone());
                self.phase = LoadPhase::Files;

                vec![Command::LoadPrFiles {
                    session: self.id,
                    repo: self.repo.clone(),
                    number: self.number,
                }]
            }
            Event::PrFilesPage {
                session,
                files,
                done,
            } if *session == self.id => {
                self.files.extend(files.iter().cloned());
                self.row_model = build(&self.files);

                if *done {
                    // The diff on screen now corresponds to the head we loaded
                    // it for, so a later metadata reply can detect a move.
                    self.diff_head_sha = self
                        .metadata
                        .as_ref()
                        .map(|metadata| metadata.head_sha.clone());
                    self.head_moved = false;
                    self.phase = LoadPhase::Ready;
                } else {
                    self.phase = LoadPhase::Files;
                }

                Vec::new()
            }
            Event::Failed {
                session: Some(session),
                error,
            } if *session == self.id => {
                self.error = Some(error.clone());
                self.phase = LoadPhase::Failed;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Re-fetch the PR from scratch, which re-fetches the files too.
    ///
    /// The previous diff is dropped immediately so stale rows are never shown
    /// against a new head; `diff_head_sha` is kept so the metadata reply can
    /// detect that the head moved.
    pub fn refresh(&mut self) -> Vec<Command> {
        self.head_moved = false;
        self.files.clear();
        self.row_model = build(&[]);
        self.phase = LoadPhase::Header;
        self.error = None;

        vec![Command::LoadPrMetadata {
            session: self.id,
            repo: self.repo.clone(),
            number: self.number,
        }]
    }

    /// Note that the session file needs writing; the next [`PrSession::tick`]
    /// past the debounce window writes it.
    pub fn mark_session_dirty(&mut self, now: Instant) {
        self.writer.mark_dirty(now);
    }

    /// Write the session file once the debounce window has elapsed.
    ///
    /// Called once per frame; the writer coalesces a burst of toggles into one
    /// write 250 ms after the last change.
    pub fn tick(&mut self, now: Instant) {
        if self.writer.should_flush(now) {
            self.write_session();
        }
    }

    /// Write any pending change now — used on exit, regardless of the debounce.
    pub fn flush_session(&mut self) {
        if self.writer.is_dirty() {
            self.write_session();
        }
    }

    /// The [`SessionFile`] this session would persist.
    fn session_file(&self) -> SessionFile {
        SessionFile {
            head_sha: self.diff_head_sha.clone(),
            viewed: self.viewed.clone(),
            // Filled by Task 40; a write here must not clobber a saved draft.
            draft: None,
            composer_draft: None,
            last_opened_file: self.last_opened_file.clone(),
            collapsed_dirs: self.collapsed_dirs.clone(),
        }
    }

    /// Write the session file atomically, reporting but not retrying a failure.
    fn write_session(&mut self) {
        let file = self.session_file();
        if let Err(error) = session::save(&self.root, &self.host, &self.repo, self.number, &file) {
            tracing::warn!(%error, "could not save the PR session");
        }
        self.writer.flushed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::error::GhError;
    use crate::runtime::{Command, Event, SessionId};
    use crate::test_support::{loaded, meta, repo};

    #[test]
    fn metadata_then_files_then_ready() {
        let (mut s, cmds) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        assert!(matches!(&cmds[..], [Command::LoadPrMetadata { .. }]));
        let cmds = s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        assert!(cmds
            .iter()
            .any(|c| matches!(c, Command::LoadPrFiles { .. })));
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: false,
        });
        assert!(matches!(s.phase, LoadPhase::Files));
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![loaded("b.rs", "@@ -1 +1 @@\n-a\n+b\n")],
            done: true,
        });
        assert!(matches!(s.phase, LoadPhase::Ready));
        assert_eq!(s.row_model.file_ranges.len(), 2);
        assert_eq!(s.diff_head_sha.as_deref(), Some("sha1"));
    }

    #[test]
    fn stale_session_events_are_dropped() {
        let (mut s, _) = PrSession::open(SessionId(2), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("old"),
        });
        assert!(s.metadata.is_none());
        s.on_event(&Event::Failed {
            session: Some(SessionId(1)),
            error: GhError::NotFound,
        });
        assert!(s.error.is_none());
    }

    #[test]
    fn refresh_detects_a_moved_head() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![],
            done: true,
        });
        s.refresh();
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha2"),
        });
        assert!(s.head_moved);
    }

    #[test]
    fn refresh_clears_a_moved_head() {
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha1"),
        });
        s.on_event(&Event::PrFilesPage {
            session: SessionId(1),
            files: vec![],
            done: true,
        });
        s.on_event(&Event::PrMetadataLoaded {
            session: SessionId(1),
            meta: meta("sha2"),
        });
        assert!(s.head_moved);

        s.refresh();
        assert!(!s.head_moved);
    }

    #[test]
    fn viewed_and_collapsed_changes_are_debounced_then_persisted() {
        use crate::test_support::{file, tempdir};
        use std::time::Duration;

        let dir = tempdir();
        let (mut s, _) = PrSession::open(SessionId(1), "github.com".into(), repo(), 7);
        // Point the session at a temp config root so the test never touches the
        // real one (the field is private, but this module owns it).
        s.root = dir.path().to_path_buf();

        let a = file("src/a.rs");
        s.viewed.set(&a, true);
        s.collapsed_dirs.insert("src".into());

        let t0 = Instant::now();
        s.mark_session_dirty(t0);
        s.tick(t0);
        assert!(
            crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
                .unwrap()
                .is_none(),
            "nothing is written before the debounce elapses"
        );

        s.tick(t0 + Duration::from_millis(300));
        let saved = crate::persist::session::load(dir.path(), "github.com", &repo(), 7)
            .unwrap()
            .unwrap();
        assert!(saved.viewed.is_viewed(&a));
        assert!(saved.collapsed_dirs.contains("src"));
    }
}
