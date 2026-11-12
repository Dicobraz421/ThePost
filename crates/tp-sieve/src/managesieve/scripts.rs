//! Команды скриптов ManageSieve (RFC 5804 §2.5–2.12; SIEVE-09) поверх [`SieveStore`](tp_contracts::sieve::SieveStore).
//!
//! Все обращения — в контексте организации и учётной записи вошедшего субъекта. Текст проверяется до записи
//! ([`compile`] с лимитами секции `sieve`): хранилище его не разбирает. Коды ответов — RFC 5804 §1.3.

use tp_contracts::Error;
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::valid_script_name;

use super::Transport;
use super::proto::{Arg, put_string};
use super::session::Session;
use crate::compile::compile;

/// Команды, доступные после входа.
pub(super) fn is_command(name: &str) -> bool {
    matches!(
        name,
        "PUTSCRIPT"
            | "CHECKSCRIPT"
            | "LISTSCRIPTS"
            | "SETACTIVE"
            | "GETSCRIPT"
            | "DELETESCRIPT"
            | "RENAMESCRIPT"
            | "HAVESPACE"
    )
}

/// Код ответа на `Error::Conflict` хранилища.
#[derive(Debug, Clone, Copy)]
enum Conflict {
    /// Удаление активного скрипта.
    Active,
    /// Имя занято.
    Exists,
}

impl<T: Transport> Session<T> {
    /// Команда из [`is_command`].
    pub(super) async fn script_command(&mut self, name: &str, args: &[Arg]) {
        let Some(scope) = self.account else {
            self.no("Authenticate first");
            return;
        };
        let mut strings = Vec::with_capacity(args.len());
        for arg in args {
            if let Arg::Str(bytes) = arg {
                let Ok(s) = std::str::from_utf8(bytes) else {
                    self.no("Invalid UTF-8");
                    return;
                };
                strings.push(s);
            }
        }
        match (name, args, strings.as_slice()) {
            ("PUTSCRIPT", [Arg::Str(_), Arg::Str(_)], &[name, text]) => {
                self.put(scope, name, text).await;
            }
            ("CHECKSCRIPT", [Arg::Str(_)], &[text]) => {
                if self.valid(text) {
                    self.ok("Script is valid");
                }
            }
            ("LISTSCRIPTS", [], _) => self.list(scope).await,
            ("SETACTIVE", [Arg::Str(_)], &[name]) => {
                // Пустое имя снимает активность со всех скриптов.
                let name = Some(name).filter(|n| !n.is_empty());
                match self.ctx.deps.store.activate(scope, name).await {
                    Ok(_) => self.ok("Setactive completed"),
                    Err(e) => self.store_error(&e, Conflict::Exists),
                }
            }
            ("GETSCRIPT", [Arg::Str(_)], &[name]) => {
                match self.ctx.deps.store.get(scope, name).await {
                    Ok(Some(text)) => {
                        self.out
                            .extend_from_slice(format!("{{{}}}\r\n", text.len()).as_bytes());
                        self.out.extend_from_slice(text.as_bytes());
                        self.out.extend_from_slice(b"\r\n");
                        self.ok("Getscript completed");
                    }
                    Ok(None) => self.store_error(&Error::NotFound, Conflict::Exists),
                    Err(e) => self.store_error(&e, Conflict::Exists),
                }
            }
            ("DELETESCRIPT", [Arg::Str(_)], &[name]) => {
                match self.ctx.deps.store.delete(scope, name).await {
                    Ok(_) => self.ok("Deletescript completed"),
                    Err(e) => self.store_error(&e, Conflict::Active),
                }
            }
            ("RENAMESCRIPT", [Arg::Str(_), Arg::Str(_)], &[from, to]) => {
                if !valid_script_name(to) {
                    self.no("Invalid script name");
                    return;
                }
                match self.ctx.deps.store.rename(scope, from, to).await {
                    Ok(_) => self.ok("Renamescript completed"),
                    Err(e) => self.store_error(&e, Conflict::Exists),
                }
            }
            ("HAVESPACE", [Arg::Str(_), Arg::Num(size)], &[name]) => {
                self.have_space(scope, name, *size).await;
            }
            _ => self.no("Invalid arguments"),
        }
    }

    /// Проверяет размер и текст скрипта; при ошибке отвечает `NO` (с позициями ошибок) и возвращает `false`.
    fn valid(&mut self, text: &str) -> bool {
        if text.len() > self.ctx.deps.store.limits().max_script_size {
            self.no_code("QUOTA/MAXSIZE", "Script is too large");
            return false;
        }
        match compile(text.as_bytes(), &self.ctx.config.limits()) {
            Ok(_) => true,
            Err(diagnostics) => {
                let lines: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
                self.no(&lines.join("\r\n"));
                false
            }
        }
    }

    async fn put(&mut self, scope: AccountScope, name: &str, text: &str) {
        if !valid_script_name(name) {
            self.no("Invalid script name");
            return;
        }
        if !self.valid(text) {
            return;
        }
        match self.ctx.deps.store.put(scope, name, text.to_owned()).await {
            Ok(_) => self.ok("Putscript completed"),
            Err(e) => self.store_error(&e, Conflict::Exists),
        }
    }

    async fn list(&mut self, scope: AccountScope) {
        match self.ctx.deps.store.list(scope).await {
            Ok(scripts) => {
                for script in scripts {
                    put_string(&mut self.out, &script.name);
                    if script.active {
                        self.out.extend_from_slice(b" ACTIVE");
                    }
                    self.out.extend_from_slice(b"\r\n");
                }
                self.ok("Listscripts completed");
            }
            Err(e) => self.store_error(&e, Conflict::Exists),
        }
    }

    /// `HAVESPACE`: поместится ли скрипт `name` размера `size` (лимиты хранилища).
    async fn have_space(&mut self, scope: AccountScope, name: &str, size: u64) {
        if !valid_script_name(name) {
            self.no("Invalid script name");
            return;
        }
        let limits = self.ctx.deps.store.limits();
        if size > limits.max_script_size as u64 {
            self.no_code("QUOTA/MAXSIZE", "Script is too large");
            return;
        }
        match self.ctx.deps.store.list(scope).await {
            Ok(scripts)
                if scripts.len() >= limits.max_scripts
                    && !scripts.iter().any(|s| s.name == name) =>
            {
                self.no_code("QUOTA/MAXSCRIPTS", "Too many scripts");
            }
            Ok(_) => self.ok("Havespace completed"),
            Err(e) => self.store_error(&e, Conflict::Exists),
        }
    }

    /// Ответ на ошибку хранилища (CONTRACTS.md#Sieve → коды RFC 5804 §1.3).
    fn store_error(&mut self, e: &Error, conflict: Conflict) {
        match (e, conflict) {
            (Error::NotFound, _) => self.no_code("NONEXISTENT", "Script does not exist"),
            (Error::Conflict, Conflict::Active) => {
                self.no_code("ACTIVE", "Script is active; deactivate it first");
            }
            (Error::Conflict, Conflict::Exists) => {
                self.no_code("ALREADYEXISTS", "Script with this name already exists");
            }
            // Размер проверен до записи: остаётся число скриптов.
            (Error::QuotaExceeded, _) => self.no_code("QUOTA/MAXSCRIPTS", "Too many scripts"),
            (Error::Invalid(_), _) => self.no("Invalid script name"),
            _ => self.unavailable(e),
        }
    }
}
