use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Некорректный адрес узла: {0}")]
    InvalidNodeUrl(String),
    #[error(
        "Для передачи мастер-токена требуется HTTPS. HTTP можно включить явно только для доверенной сети."
    )]
    InsecureTransport,
    #[error("Мастер-токен не может быть пустым.")]
    EmptyMasterToken,
    #[error("Узел отклонил мастер-токен.")]
    InvalidCredentials,
    #[error("Узел не поддерживает безопасную desktop-сессию.")]
    UnsupportedNode,
    #[error("Узел временно недоступен: {0}")]
    NodeUnavailable(String),
    #[error("Узел вернул некорректный ответ: {0}")]
    InvalidNodeResponse(String),
    #[error("Пароль локального хранилища должен содержать не менее 10 символов.")]
    WeakVaultPassword,
    #[error("Неверный пароль локального хранилища или файл хранилища повреждён.")]
    InvalidVaultPassword,
    #[error("Локальное хранилище ещё не создано.")]
    VaultMissing,
    #[error("Активная сессия узла отсутствует. Разблокируйте локальное хранилище.")]
    SessionMissing,
    #[error("Некорректный offline package: {0}")]
    InvalidPackage(String),
    #[error("Не удалось скачать offline package: {0}")]
    Download(String),
    #[error("Requested byte range is not satisfiable")]
    RangeNotSatisfiable,
    #[error("Ошибка локального хранилища: {0}")]
    Storage(String),
    #[error("Внутренняя ошибка приложения: {0}")]
    Internal(String),
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
