use std::{collections::HashMap, sync::Arc};

use turso_parser::ast::{self, Expr, Upsert};

use crate::{
    bail_parse_error,
    error::SQLITE_CONSTRAINT_NOTNULL,
    schema::{Index, IndexColumn, Schema, Table},
    translate::{
        emitter::{
            emit_cdc_full_record, emit_cdc_insns, emit_cdc_patch_record, OperationMode, Resolver,
        },
        expr::{
            emit_returning_results, translate_expr, translate_expr_no_constant_opt,
            NoConstantOptReason, ReturningValueRegisters,
        },
        insert::{Insertion, ROWID_COLUMN},
        plan::ResultSetColumn,
    },
    util::normalize_ident,
    vdbe::{
        builder::ProgramBuilder,
        insn::{IdxInsertFlags, InsertFlags, Insn},
        BranchOffset,
    },
};

// What we extract from each ON CONFLICT target term
#[derive(Debug, Clone)]
pub struct ConflictTarget {
    col_name: String,
    collate: Option<String>,
}

// Extract `(column, optional_collate)` from an ON CONFLICT target Expr.
// Accepts: Id, Qualified, DoublyQualified, Parenthesized, Collate
fn extract_target_key(e: &ast::Expr) -> Option<ConflictTarget> {
    match e {
        // expr COLLATE c: carry c and keep descending into expr
        ast::Expr::Collate(inner, c) => {
            let mut tk = extract_target_key(inner.as_ref())?;
            let cstr = match c {
                ast::Name::Ident(s) => s.as_str(),
                _ => return None,
            };
            tk.collate = Some(cstr.to_ascii_lowercase());
            Some(tk)
        }
        ast::Expr::Parenthesized(v) if v.len() == 1 => extract_target_key(&v[0]),
        // Bare identifier
        ast::Expr::Id(ast::Name::Ident(name)) => Some(ConflictTarget {
            col_name: normalize_ident(name),
            collate: None,
        }),
        // t.a or db.t.a
        ast::Expr::Qualified(_, col) | ast::Expr::DoublyQualified(_, _, col) => {
            let cname = match col {
                ast::Name::Ident(s) => s.as_str(),
                _ => return None,
            };
            Some(ConflictTarget {
                col_name: normalize_ident(cname),
                collate: None,
            })
        }
        _ => None,
    }
}

// Return the index key’s effective collation.
// If `idx_col.collation` is None, fall back to the column default or "BINARY".
fn effective_collation_for_index_col(idx_col: &IndexColumn, table: &Table) -> String {
    if let Some(c) = idx_col.collation.as_ref() {
        return c.to_string();
    }
    // Otherwise use the table default, or default to BINARY
    table
        .get_column_by_name(&idx_col.name)
        .map(|s| {
            s.1.collation
                .map(|c| c.to_string().to_ascii_lowercase())
                .unwrap_or_else(|| "binary".to_string())
        })
        .unwrap_or_else(|| "binary".to_string())
}

/// Column names in the expressions of a DO UPDATE refer to the original unchanged value of the column, before the attempted INSERT.
/// To use the value that would have been inserted had the constraint not failed, add the special "excluded." table qualifier to the column name.
/// https://sqlite.org/lang_upsert.html
///
/// Rewrite EXCLUDED.x to Expr::Register(<reg of x from insertion>)
pub fn rewrite_excluded_in_expr(expr: &mut Expr, insertion: &Insertion) {
    match expr {
        // EXCLUDED.x accept Qualified with left=excluded
        Expr::Qualified(ns, col) if ns.as_str().eq_ignore_ascii_case("excluded") => {
            let cname = match col {
                ast::Name::Ident(s) => s.as_str(),
                _ => return,
            };
            let src = insertion.get_col_mapping_by_name(cname).register;
            *expr = Expr::Register(src);
        }

        Expr::Collate(inner, _) => rewrite_excluded_in_expr(inner, insertion),
        Expr::Parenthesized(v) => {
            for e in v {
                rewrite_excluded_in_expr(e, insertion)
            }
        }
        Expr::Between {
            lhs, start, end, ..
        } => {
            rewrite_excluded_in_expr(lhs, insertion);
            rewrite_excluded_in_expr(start, insertion);
            rewrite_excluded_in_expr(end, insertion);
        }
        Expr::Binary(l, _, r) => {
            rewrite_excluded_in_expr(l, insertion);
            rewrite_excluded_in_expr(r, insertion);
        }
        Expr::Case {
            base,
            when_then_pairs,
            else_expr,
        } => {
            if let Some(b) = base {
                rewrite_excluded_in_expr(b, insertion)
            }
            for (w, t) in when_then_pairs.iter_mut() {
                rewrite_excluded_in_expr(w, insertion);
                rewrite_excluded_in_expr(t, insertion);
            }
            if let Some(e) = else_expr {
                rewrite_excluded_in_expr(e, insertion)
            }
        }
        Expr::Cast { expr: inner, .. } => rewrite_excluded_in_expr(inner, insertion),
        Expr::FunctionCall {
            args,
            order_by,
            filter_over,
            ..
        } => {
            for a in args {
                rewrite_excluded_in_expr(a, insertion)
            }
            for sc in order_by {
                rewrite_excluded_in_expr(&mut sc.expr, insertion)
            }
            if let Some(ex) = &mut filter_over.filter_clause {
                rewrite_excluded_in_expr(ex, insertion)
            }
        }
        Expr::InList { lhs, rhs, .. } => {
            rewrite_excluded_in_expr(lhs, insertion);
            for e in rhs {
                rewrite_excluded_in_expr(e, insertion)
            }
        }
        Expr::InSelect { lhs, .. } => rewrite_excluded_in_expr(lhs, insertion),
        Expr::InTable { lhs, .. } => rewrite_excluded_in_expr(lhs, insertion),
        Expr::IsNull(inner) => rewrite_excluded_in_expr(inner, insertion),
        Expr::Like {
            lhs, rhs, escape, ..
        } => {
            rewrite_excluded_in_expr(lhs, insertion);
            rewrite_excluded_in_expr(rhs, insertion);
            if let Some(e) = escape {
                rewrite_excluded_in_expr(e, insertion)
            }
        }
        Expr::NotNull(inner) => rewrite_excluded_in_expr(inner, insertion),
        Expr::Unary(_, inner) => rewrite_excluded_in_expr(inner, insertion),
        _ => {}
    }
}

pub fn upsert_matches_pk(upsert: &Upsert, table: &Table) -> bool {
    // Omitted target is automatic match for primary key
    let Some(t) = upsert.index.as_ref() else {
        return true;
    };
    if !t.targets.len().eq(&1) {
        return false;
    }
    let pk = table
        .columns()
        .iter()
        .find(|c| c.is_rowid_alias || c.primary_key)
        .unwrap_or(ROWID_COLUMN);
    extract_target_key(&t.targets[0].expr).is_some_and(|tk| {
        tk.col_name
            .eq_ignore_ascii_case(pk.name.as_ref().unwrap_or(&String::new()))
    })
}

#[derive(Hash, Debug, Eq, PartialEq, Clone)]
pub struct KeySig {
    name: String,
    coll: String,
}

/// Match ON CONFLICT target to a UNIQUE index, ignoring order, requiring exact
/// coverage, and honoring collations. `table` is used to derive effective collation.
pub fn upsert_matches_index(upsert: &Upsert, index: &Index, table: &Table) -> bool {
    let Some(target) = upsert.index.as_ref() else {
        // catch-all ON CONFLICT DO
        return true;
    };
    // if not unique or column count differs, no match
    if !index.unique || target.targets.len() != index.columns.len() {
        return false;
    }

    let mut need: HashMap<KeySig, usize> = HashMap::new();
    for ic in &index.columns {
        let sig = KeySig {
            name: normalize_ident(&ic.name).to_string(),
            coll: effective_collation_for_index_col(ic, table),
        };
        *need.entry(sig).or_insert(0) += 1;
    }

    // Consume from the multiset using target entries, order-insensitive
    for te in &target.targets {
        let tk = match extract_target_key(&te.expr) {
            Some(x) => x,
            None => return false, // not a simple column ref
        };

        // Candidate signatures for this target:
        // If target specifies COLLATE, require exact match on (name, coll).
        // Otherwise, accept any collation currently present for that name.
        let mut matched = false;
        if let Some(ref coll) = tk.collate {
            let sig = KeySig {
                name: tk.col_name.to_string(),
                coll: coll.clone(),
            };
            if let Some(cnt) = need.get_mut(&sig) {
                *cnt -= 1;
                if *cnt == 0 {
                    need.remove(&sig);
                }
                matched = true;
            }
        } else {
            // Try any available collation for this column name
            if let Some((sig, cnt)) = need
                .iter_mut()
                .find(|(k, _)| k.name.eq_ignore_ascii_case(&tk.col_name))
            {
                *cnt -= 1;
                if *cnt == 0 {
                    let key = sig.clone();
                    need.remove(&key);
                }
                matched = true;
            }
        }
        if !matched {
            return false;
        }
    }
    // All targets matched exactly.
    need.is_empty()
}

#[allow(clippy::too_many_arguments)]
/// https://sqlite.org/lang_upsert.html
/// Column names in the expressions of a DO UPDATE refer to the original unchanged value of the column, before the attempted INSERT.
/// To use the value that would have been inserted had the constraint not failed, add the special "excluded." table qualifier to the column name.
pub fn emit_upsert(
    program: &mut ProgramBuilder,
    schema: &Schema,
    table: &Table,
    tbl_cursor_id: usize,
    conflict_rowid_reg: usize,
    set_pairs: &mut [(usize, Box<ast::Expr>)],
    where_clause: &mut Option<Box<ast::Expr>>,
    resolver: &Resolver,
    idx_cursors: &[(&String, usize, usize)],
    returning: &mut [ResultSetColumn],
    cdc_cursor_id: Option<usize>,
    row_done_label: BranchOffset,
) -> crate::Result<()> {
    // Seek and snapshot current row
    program.emit_insn(Insn::SeekRowid {
        cursor_id: tbl_cursor_id,
        src_reg: conflict_rowid_reg,
        target_pc: row_done_label,
    });
    let num_cols = table.columns().len();
    let current_start = program.alloc_registers(num_cols);
    for (i, col) in table.columns().iter().enumerate() {
        if col.is_rowid_alias {
            program.emit_insn(Insn::RowId {
                cursor_id: tbl_cursor_id,
                dest: current_start + i,
            });
        } else {
            program.emit_insn(Insn::Column {
                cursor_id: tbl_cursor_id,
                column: i,
                dest: current_start + i,
                default: None,
            });
        }
    }

    // Keep BEFORE snapshot if needed
    let before_start = if cdc_cursor_id.is_some() || !idx_cursors.is_empty() {
        let s = program.alloc_registers(num_cols);
        program.emit_insn(Insn::Copy {
            src_reg: current_start,
            dst_reg: s,
            extra_amount: num_cols - 1,
        });
        Some(s)
    } else {
        None
    };

    // Build NEW image = copy of CURRENT
    let new_start = program.alloc_registers(num_cols);
    program.emit_insn(Insn::Copy {
        src_reg: current_start,
        dst_reg: new_start,
        extra_amount: num_cols - 1,
    });

    // rewrite target-table refs -> registers from current snapshot
    let rewrite_target = |e: &mut ast::Expr| {
        rewrite_target_cols_to_current_row(e, table, current_start, conflict_rowid_reg);
    };

    // WHERE predicate
    if let Some(pred) = where_clause.as_mut() {
        rewrite_target(pred);
        let pr = program.alloc_register();
        translate_expr(program, None, pred, pr, resolver)?;
        program.emit_insn(Insn::IfNot {
            reg: pr,
            target_pc: row_done_label,
            jump_if_null: true,
        });
    }

    // Apply SET into new_start, read from current_start via rewrites
    for (col_idx, expr) in set_pairs.iter_mut() {
        rewrite_target(expr);
        translate_expr_no_constant_opt(
            program,
            None,
            expr,
            new_start + *col_idx,
            resolver,
            NoConstantOptReason::RegisterReuse,
        )?;

        let col = &table.columns()[*col_idx];
        if col.notnull && !col.is_rowid_alias {
            program.emit_insn(Insn::HaltIfNull {
                target_reg: new_start + *col_idx,
                err_code: SQLITE_CONSTRAINT_NOTNULL,
                description: format!("{}.{}", table.get_name(), col.name.as_ref().unwrap()),
            });
        }
    }

    // STRICT type-check on NEW snapshot if applicable
    if let Some(bt) = table.btree() {
        if bt.is_strict {
            program.emit_insn(Insn::TypeCheck {
                start_reg: new_start,
                count: num_cols,
                check_generated: true,
                table_reference: Arc::clone(&bt),
            });
        }
    }

    // Rebuild indexes (delete old keys using BEFORE, insert new keys using NEW)
    if let Some(before) = before_start {
        for (idx_name, _root, idx_cid) in idx_cursors {
            let idx_meta = schema
                .get_index(table.get_name(), idx_name)
                .expect("index exists");
            let k = idx_meta.columns.len();

            // delete old
            let del = program.alloc_registers(k + 1);
            for (i, ic) in idx_meta.columns.iter().enumerate() {
                let (ci, _) = table.get_column_by_name(&ic.name).unwrap();
                program.emit_insn(Insn::Copy {
                    src_reg: before + ci,
                    dst_reg: del + i,
                    extra_amount: 0,
                });
            }
            program.emit_insn(Insn::Copy {
                src_reg: conflict_rowid_reg,
                dst_reg: del + k,
                extra_amount: 0,
            });
            program.emit_insn(Insn::IdxDelete {
                start_reg: del,
                num_regs: k + 1,
                cursor_id: *idx_cid,
                raise_error_if_no_matching_entry: false,
            });

            // insert new
            let ins = program.alloc_registers(k + 1);
            for (i, ic) in idx_meta.columns.iter().enumerate() {
                let (ci, _) = table.get_column_by_name(&ic.name).unwrap();
                program.emit_insn(Insn::Copy {
                    src_reg: new_start + ci,
                    dst_reg: ins + i,
                    extra_amount: 0,
                });
            }
            program.emit_insn(Insn::Copy {
                src_reg: conflict_rowid_reg,
                dst_reg: ins + k,
                extra_amount: 0,
            });

            let rec = program.alloc_register();
            program.emit_insn(Insn::MakeRecord {
                start_reg: ins,
                count: k + 1,
                dest_reg: rec,
                index_name: Some((*idx_name).clone()),
            });
            program.emit_insn(Insn::IdxInsert {
                cursor_id: *idx_cid,
                record_reg: rec,
                unpacked_start: Some(ins),
                unpacked_count: Some((k + 1) as u16),
                flags: IdxInsertFlags::new().nchange(true),
            });
        }
    }

    // Write table row (same rowid, new payload)
    let rec = program.alloc_register();
    program.emit_insn(Insn::MakeRecord {
        start_reg: new_start,
        count: num_cols,
        dest_reg: rec,
        index_name: None,
    });
    program.emit_insn(Insn::Insert {
        cursor: tbl_cursor_id,
        key_reg: conflict_rowid_reg,
        record_reg: rec,
        flag: InsertFlags::new(),
        table_name: table.get_name().to_string(),
    });

    if let Some(cdc_id) = cdc_cursor_id {
        let after_rec = if program.capture_data_changes_mode().has_after() {
            Some(emit_cdc_patch_record(
                program,
                table,
                new_start,
                rec,
                conflict_rowid_reg,
            ))
        } else {
            None
        };
        // Build BEFORE if needed
        let before_rec = if program.capture_data_changes_mode().has_before() {
            Some(emit_cdc_full_record(
                program,
                table.columns(),
                tbl_cursor_id,
                conflict_rowid_reg,
            ))
        } else {
            None
        };
        emit_cdc_insns(
            program,
            resolver,
            OperationMode::UPDATE,
            cdc_id,
            conflict_rowid_reg,
            before_rec,
            after_rec,
            None,
            table.get_name(),
        )?;
    }

    if !returning.is_empty() {
        let regs = ReturningValueRegisters {
            rowid_register: conflict_rowid_reg,
            columns_start_register: new_start,
            num_columns: num_cols,
        };

        emit_returning_results(program, returning, &regs)?;
    }
    program.emit_insn(Insn::Goto {
        target_pc: row_done_label,
    });
    Ok(())
}

/// Normalizes a list of SET items into (pos_in_table, Expr) pairs using the same
/// rules as UPDATE. `set_items`
///
/// `rewrite_excluded_in_expr` must be run on each RHS first.
pub fn collect_set_clauses_for_upsert(
    table: &Table,
    set_items: &mut [ast::Set],
    insertion: &Insertion, // for EXCLUDED.*
) -> crate::Result<Vec<(usize, Box<ast::Expr>)>> {
    let lookup: HashMap<String, usize> = table
        .columns()
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.name.as_ref().map(|n| (n.to_lowercase(), i)))
        .collect();

    let mut out: Vec<(usize, Box<ast::Expr>)> = vec![];

    for set in set_items {
        let values: Vec<Box<ast::Expr>> = match set.expr.as_ref() {
            ast::Expr::Parenthesized(v) => v.clone(),
            e => vec![e.clone().into()],
        };
        if set.col_names.len() != values.len() {
            bail_parse_error!(
                "{} columns assigned {} values",
                set.col_names.len(),
                values.len()
            );
        }
        for (cn, mut e) in set.col_names.iter().zip(values.into_iter()) {
            rewrite_excluded_in_expr(&mut e, insertion);
            let Some(idx) = lookup.get(&normalize_ident(cn.as_str())) else {
                bail_parse_error!("no such column: {}", cn);
            };
            // last one wins
            if let Some(existing) = out.iter_mut().find(|(i, _)| *i == *idx) {
                existing.1 = e;
            } else {
                out.push((*idx, e));
            }
        }
    }
    Ok(out)
}

/// In Upsert, we load the target row into a set of registers.
/// table: testing(a,b,c);
/// 1. Of the Row in question that has conflicted: load a, b, c into registers R1, R2, R3.
/// 2. instead of rewriting all the Expr::Id("a") to Expr::Column{..}, we can just rewrite
///    it to Expr::Register(R1), and any columns referenced in the UPDATE DO set clause
///    can then have the expression translated into that register.
///
/// Rewrite references to the *target table* columns to the registers that hold
/// the original row image already loaded in `emit_upsert`.
fn rewrite_target_cols_to_current_row(
    expr: &mut ast::Expr,
    table: &Table,
    current_start: usize,
    conflict_rowid_reg: usize,
) {
    use ast::Expr::*;

    // Helper: map a column name to (is_rowid, register)
    let col_reg = |name: &str| -> Option<usize> {
        if name.eq_ignore_ascii_case("rowid") {
            return Some(conflict_rowid_reg);
        }
        let (idx, col) = table.get_column_by_name(&normalize_ident(name))?;
        if col.is_rowid_alias {
            // You loaded alias value into current_start + idx
            return Some(current_start + idx);
        }
        Some(current_start + idx)
    };

    match expr {
        // tbl.col: only rewrite if it names this table
        ast::Expr::Qualified(left, col) => {
            let q = left.as_str();
            if !q.eq_ignore_ascii_case("excluded") && q.eq_ignore_ascii_case(table.get_name()) {
                if let ast::Name::Ident(c) = col {
                    if let Some(reg) = col_reg(c.as_str()) {
                        *expr = Register(reg);
                    }
                }
            }
        }
        ast::Expr::Id(ast::Name::Ident(name)) => {
            if let Some(reg) = col_reg(name.as_str()) {
                *expr = Register(reg);
            }
        }
        ast::Expr::RowId { .. } => {
            *expr = Register(conflict_rowid_reg);
        }

        // Keep walking for composite expressions
        Collate(inner, _) => {
            rewrite_target_cols_to_current_row(inner, table, current_start, conflict_rowid_reg)
        }
        Parenthesized(v) => {
            for e in v {
                rewrite_target_cols_to_current_row(e, table, current_start, conflict_rowid_reg)
            }
        }
        Between {
            lhs, start, end, ..
        } => {
            rewrite_target_cols_to_current_row(lhs, table, current_start, conflict_rowid_reg);
            rewrite_target_cols_to_current_row(start, table, current_start, conflict_rowid_reg);
            rewrite_target_cols_to_current_row(end, table, current_start, conflict_rowid_reg);
        }
        Binary(l, _, r) => {
            rewrite_target_cols_to_current_row(l, table, current_start, conflict_rowid_reg);
            rewrite_target_cols_to_current_row(r, table, current_start, conflict_rowid_reg);
        }
        Case {
            base,
            when_then_pairs,
            else_expr,
        } => {
            if let Some(b) = base {
                rewrite_target_cols_to_current_row(b, table, current_start, conflict_rowid_reg)
            }
            for (w, t) in when_then_pairs.iter_mut() {
                rewrite_target_cols_to_current_row(w, table, current_start, conflict_rowid_reg);
                rewrite_target_cols_to_current_row(t, table, current_start, conflict_rowid_reg);
            }
            if let Some(e) = else_expr {
                rewrite_target_cols_to_current_row(e, table, current_start, conflict_rowid_reg)
            }
        }
        Cast { expr: inner, .. } => {
            rewrite_target_cols_to_current_row(inner, table, current_start, conflict_rowid_reg)
        }
        FunctionCall {
            args,
            order_by,
            filter_over,
            ..
        } => {
            for a in args {
                rewrite_target_cols_to_current_row(a, table, current_start, conflict_rowid_reg)
            }
            for sc in order_by {
                rewrite_target_cols_to_current_row(
                    &mut sc.expr,
                    table,
                    current_start,
                    conflict_rowid_reg,
                )
            }
            if let Some(ref mut f) = &mut filter_over.filter_clause {
                rewrite_target_cols_to_current_row(f, table, current_start, conflict_rowid_reg)
            }
        }
        InList { lhs, rhs, .. } => {
            rewrite_target_cols_to_current_row(lhs, table, current_start, conflict_rowid_reg);
            for e in rhs {
                rewrite_target_cols_to_current_row(e, table, current_start, conflict_rowid_reg)
            }
        }
        InSelect { lhs, .. } => {
            rewrite_target_cols_to_current_row(lhs, table, current_start, conflict_rowid_reg)
        }
        InTable { lhs, .. } => {
            rewrite_target_cols_to_current_row(lhs, table, current_start, conflict_rowid_reg)
        }
        IsNull(inner) => {
            rewrite_target_cols_to_current_row(inner, table, current_start, conflict_rowid_reg)
        }
        Like {
            lhs, rhs, escape, ..
        } => {
            rewrite_target_cols_to_current_row(lhs, table, current_start, conflict_rowid_reg);
            rewrite_target_cols_to_current_row(rhs, table, current_start, conflict_rowid_reg);
            if let Some(e) = escape {
                rewrite_target_cols_to_current_row(e, table, current_start, conflict_rowid_reg)
            }
        }
        NotNull(inner) => {
            rewrite_target_cols_to_current_row(inner, table, current_start, conflict_rowid_reg)
        }
        Unary(_, inner) => {
            rewrite_target_cols_to_current_row(inner, table, current_start, conflict_rowid_reg)
        }
        _ => {}
    }
}
