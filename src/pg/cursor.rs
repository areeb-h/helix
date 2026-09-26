//! A result read a page at a time — the streaming form ADR 0044 had listed as its last
//! functional honest cost (addendum 2026-09-26).
//!
//! WHAT IT IS ON THE WIRE. The statement is prepared and bound exactly as `query` binds it —
//! the same plan, the same cache, the same formats — but to a NAMED portal, and Executed with a
//! row limit. The server answers that many rows and `PortalSuspended`; each `next()` Executes
//! the same portal again, one round trip a page, until an Execute runs to its end and answers
//! `CommandComplete`. A portal lives until its transaction ends, so a cursor is always inside
//! one: the transaction the cursor was opened on, or — opened on a connection — one begun for
//! it in the same round trip as its first page, and ended by the cursor's last page or its
//! value's drop (`mod.rs` keeps those rules; this file keeps the portal).
//!
//! WHY NOT `DECLARE CURSOR`. A SQL cursor is planned for a fast START (`cursor_tuple_fraction`),
//! which for a full read can be a slower plan than the whole read's; a portal with a row limit
//! runs the plan `query` runs, so `cursor(sql)` reads exactly what `query(sql)` reads, in
//! pages. And it costs no SQL of its own: no `DECLARE`, no `FETCH` text a prepared-statement
//! cache would fill with, no `CLOSE`.
//!
//! WHAT A PORTAL HOLDS ON TO, AND HOW IT IS LET GO OF.
//! - Its statement. The protocol's documented contract is that closing a prepared statement
//!   closes the portals bound from it, and the cache's own eviction is such a Close — so while
//!   the portal lives, the statement it was bound from is PINNED: never the one displaced to
//!   make room. (PostgreSQL 17 in fact keeps the portal, measured through an eviction and
//!   through `deallocate all`; the documented contract is the one kept.) A portal bound from
//!   the unnamed statement pins nothing: that statement is replaced by the next unnamed Parse,
//!   which inside a transaction is its own `commit` or `rollback` — or the fallback for a name a
//!   user's `PREPARE` took, where the documented contract would let the portal go.
//! - The server's memory of it. A portal run to its end is still held until it is closed or its
//!   transaction ends, and one let go of early is suspended mid-result. Inside a transaction that
//!   carries on, its Close rides at the head of the NEXT exchange, whatever that is: no round
//!   trip of its own. A transaction's end takes every portal with it, and the session forgets
//!   the pins and the owed Closes when the server says it is out of one (`Session::saw_ready`).

use std::rc::Rc;

use super::statement::{run_page, Fail, Fetch, Outcome, Owed, Portal, RowDesc, Session};
use super::types::ColBuf;

/// The portal a cursor reads, and what its pages look like.
pub struct Cursor {
    /// The number the portal's name is made from.
    portal: u64,
    /// The prepared statement the portal was bound from, pinned while the portal lives; `None`
    /// for the unnamed statement.
    statement: Option<u64>,
    /// Rows a page asks for.
    batch: u32,
    desc: Rc<RowDesc>,
    /// Whether Bind asked for the fixed-width columns in binary — the formats every page of
    /// this portal arrives in, decided once when it was bound.
    binary: bool,
    exact_float_text: bool,
    /// The first page, which arrived with the open, until `next` hands it over.
    first: Option<Vec<ColBuf>>,
    /// Nothing more to ask the portal for: it has run to its end, or a page failed.
    done: bool,
    /// The portal has been let go of (`release`): its statement unpinned, its Close owed if it
    /// needed one.
    released: bool,
}

impl Cursor {
    /// A cursor over portal `portal`, from the outcome of the exchange that bound it and read
    /// its first page — pinning the statement it was bound from.
    pub fn opened(portal: u64, batch: u32, out: Outcome, se: &mut Session) -> Result<Cursor, String> {
        // `Answer::finish` has already refused a result the server never described.
        let desc = out.desc.ok_or_else(|| "the server never described the result".to_string())?;
        if let Some(id) = out.bound {
            se.pin(id);
        }
        Ok(Cursor {
            portal,
            statement: out.bound,
            batch,
            desc,
            binary: out.binary,
            exact_float_text: se.exact_float_text(),
            first: Some(out.cols),
            done: !out.suspended,
            released: false,
        })
    }

    /// Nothing more will come: the last page has been read, or the cursor ended early.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The page that came with the open, the first time it is asked for. It is already here,
    /// so it is handed over before anything else is asked — of the session, or of whether the
    /// transaction it was read in still stands.
    pub fn take_first(&mut self) -> Option<Vec<ColBuf>> {
        self.first.take()
    }

    /// The page after the last: the result's columns, and no rows. Costs no round trip.
    pub fn empty(&self) -> Vec<ColBuf> {
        self.desc.bufs(self.binary, self.exact_float_text)
    }

    /// The next page: the one that came with the open, then one round trip each. A page that
    /// fails ends the cursor as surely as the last one does.
    pub fn next(&mut self, se: &mut Session) -> Result<Vec<ColBuf>, Fail> {
        if let Some(first) = self.take_first() {
            return Ok(first);
        }
        if self.done {
            return Ok(self.empty());
        }
        let page = run_page(se, Fetch { portal: Portal::Named(self.portal), limit: self.batch }, self.empty());
        self.done = match &page {
            Ok(out) => !out.suspended,
            Err(_) => true,
        };
        page.map(|out| out.cols)
    }

    /// Let go of the portal, once: its statement unpinned, and — when the transaction it lives
    /// in carries on (`close`) and the server still holds it — its Close owed to the next
    /// exchange. A portal whose transaction ends with it (the cursor's own, committed or rolled
    /// back next) or has failed (the rollback that has to follow takes it) needs none.
    pub fn release(&mut self, se: &mut Session, close: bool) {
        if self.released {
            return;
        }
        self.released = true;
        self.done = true;
        if close && se.status == b'T' {
            se.owe(Owed::Portal(self.portal));
        }
        if let Some(id) = self.statement {
            se.unpin(id);
        }
    }
}
