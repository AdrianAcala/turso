use crate::schema::Column;
use crate::util::columns_from_create_table_body;
use crate::{Connection, LimboError, SymbolTable, Value};
use fallible_iterator::FallibleIterator;
use limbo_ext::{ConstraintInfo, IndexInfo, OrderByInfo, ResultCode, VTabKind, VTabModuleImpl};
use limbo_sqlite3_parser::{ast, lexer::sql::Parser};
use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::{Rc, Weak};

#[derive(Clone, Debug)]
pub struct VirtualTable {
    pub name: String,
    pub args: Option<Vec<ast::Expr>>,
    pub implementation: Rc<VTabModuleImpl>,
    pub columns: Vec<Column>,
    pub kind: VTabKind,
    table_ptr: *const c_void,
    connection_ptr: RefCell<Option<*mut limbo_ext::Conn>>,
}

impl Drop for VirtualTable {
    fn drop(&mut self) {
        if let Some(conn) = self.connection_ptr.borrow_mut().take() {
            if conn.is_null() {
                return;
            }
            // free the memory for the limbo_ext::Conn itself
            let mut conn = unsafe { Box::from_raw(conn) };
            // frees the boxed Weak pointer
            conn.close();
        }
        *self.connection_ptr.borrow_mut() = None;
    }
}

impl VirtualTable {
    pub(crate) fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> IndexInfo {
        unsafe {
            IndexInfo::from_ffi((self.implementation.best_idx)(
                constraints.as_ptr(),
                constraints.len() as i32,
                order_by.as_ptr(),
                order_by.len() as i32,
            ))
        }
    }

    /// takes ownership of the provided Args
    pub(crate) fn from_args(
        tbl_name: Option<&str>,
        module_name: &str,
        args: Vec<limbo_ext::Value>,
        syms: &SymbolTable,
        kind: VTabKind,
        exprs: Option<Vec<ast::Expr>>,
    ) -> crate::Result<Rc<Self>> {
        let module = syms
            .vtab_modules
            .get(module_name)
            .ok_or(LimboError::ExtensionError(format!(
                "Virtual table module not found: {}",
                module_name
            )))?;
        if let VTabKind::VirtualTable = kind {
            if module.module_kind == VTabKind::TableValuedFunction {
                return Err(LimboError::ExtensionError(format!(
                    "{} is not a virtual table module",
                    module_name
                )));
            }
        };
        let (schema, table_ptr) = module.implementation.create(args)?;
        let mut parser = Parser::new(schema.as_bytes());
        if let ast::Cmd::Stmt(ast::Stmt::CreateTable { body, .. }) = parser.next()?.ok_or(
            LimboError::ParseError("Failed to parse schema from virtual table module".to_string()),
        )? {
            let columns = columns_from_create_table_body(&body)?;
            let vtab = Rc::new(VirtualTable {
                name: tbl_name.unwrap_or(module_name).to_owned(),
                connection_ptr: RefCell::new(None),
                implementation: module.implementation.clone(),
                columns,
                args: exprs,
                kind,
                table_ptr,
            });
            return Ok(vtab);
        }
        Err(LimboError::ParseError(
            "Failed to parse schema from virtual table module".to_string(),
        ))
    }

    /// Accepts a Weak pointer to the connection that owns the VTable, that the module
    /// can optionally use to query the other tables.
    pub fn open(&self, conn: Weak<Connection>) -> crate::Result<VirtualTableCursor> {
        // we need a Weak<Connection> to upgrade and call from the extension.
        let weak_box: *mut Weak<Connection> = Box::into_raw(Box::new(conn));
        let conn = limbo_ext::Conn::new(
            weak_box.cast(),
            crate::ext::prepare_stmt,
            crate::ext::execute,
            crate::ext::close,
        );
        let ext_conn_ptr = Box::into_raw(Box::new(conn));
        // store the leaked connection pointer on the table so it can be freed on drop
        *self.connection_ptr.borrow_mut() = Some(ext_conn_ptr);
        let cursor = unsafe { (self.implementation.open)(self.table_ptr, ext_conn_ptr) };
        VirtualTableCursor::new(cursor, self.implementation.clone())
    }

    pub fn update(&self, args: &[Value]) -> crate::Result<Option<i64>> {
        let arg_count = args.len();
        let ext_args = args.iter().map(|arg| arg.to_ffi()).collect::<Vec<_>>();
        let newrowid = 0i64;
        let rc = unsafe {
            (self.implementation.update)(
                self.table_ptr,
                arg_count as i32,
                ext_args.as_ptr(),
                &newrowid as *const _ as *mut i64,
            )
        };
        for arg in ext_args {
            unsafe {
                arg.__free_internal_type();
            }
        }
        match rc {
            ResultCode::OK => Ok(None),
            ResultCode::RowID => Ok(Some(newrowid)),
            _ => Err(LimboError::ExtensionError(rc.to_string())),
        }
    }

    pub fn destroy(&self) -> crate::Result<()> {
        let rc = unsafe { (self.implementation.destroy)(self.table_ptr) };
        match rc {
            ResultCode::OK => Ok(()),
            _ => Err(LimboError::ExtensionError(rc.to_string())),
        }
    }
}

pub struct VirtualTableCursor {
    cursor: *const c_void,
    implementation: Rc<VTabModuleImpl>,
}

impl VirtualTableCursor {
    pub fn new(cursor: *const c_void, implementation: Rc<VTabModuleImpl>) -> crate::Result<Self> {
        if cursor.is_null() {
            return Err(LimboError::InternalError(
                "VirtualTableCursor: cursor is null".into(),
            ));
        }
        Ok(Self {
            cursor,
            implementation,
        })
    }

    pub(crate) fn rowid(&self) -> i64 {
        unsafe { (self.implementation.rowid)(self.cursor) }
    }

    #[tracing::instrument(skip(self))]
    pub fn filter(
        &self,
        idx_num: i32,
        idx_str: Option<String>,
        arg_count: usize,
        args: Vec<limbo_ext::Value>,
    ) -> crate::Result<bool> {
        tracing::trace!("xFilter");
        let c_idx_str = idx_str
            .map(|s| std::ffi::CString::new(s).unwrap())
            .map(|cstr| cstr.into_raw())
            .unwrap_or(std::ptr::null_mut());
        let rc = unsafe {
            (self.implementation.filter)(
                self.cursor,
                arg_count as i32,
                args.as_ptr(),
                c_idx_str,
                idx_num,
            )
        };
        for arg in args {
            unsafe {
                arg.__free_internal_type();
            }
        }
        match rc {
            ResultCode::OK => Ok(true),
            ResultCode::EOF => Ok(false),
            _ => Err(LimboError::ExtensionError(rc.to_string())),
        }
    }

    pub fn column(&self, column: usize) -> crate::Result<Value> {
        let val = unsafe { (self.implementation.column)(self.cursor, column as u32) };
        Value::from_ffi(val)
    }

    pub fn next(&self) -> crate::Result<bool> {
        let rc = unsafe { (self.implementation.next)(self.cursor) };
        match rc {
            ResultCode::OK => Ok(true),
            ResultCode::EOF => Ok(false),
            _ => Err(LimboError::ExtensionError("Next failed".to_string())),
        }
    }
}

impl Drop for VirtualTableCursor {
    fn drop(&mut self) {
        let result = unsafe { (self.implementation.close)(self.cursor) };
        if !result.is_ok() {
            tracing::error!("Failed to close virtual table cursor");
        }
    }
}
