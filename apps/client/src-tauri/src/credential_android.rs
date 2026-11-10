use super::CredentialError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Put,
    Get,
    Delete,
}

pub(super) fn decode_reply(
    operation: Operation,
    reply: Vec<u8>,
) -> Result<Option<Vec<u8>>, CredentialError> {
    let Some((&status, payload)) = reply.split_first() else {
        return Err(CredentialError::Storage);
    };
    if status == 0 && operation == Operation::Get {
        return if (1..=8192).contains(&payload.len()) {
            Ok(Some(payload.to_vec()))
        } else {
            Err(CredentialError::Storage)
        };
    }
    if !payload.is_empty() {
        return Err(CredentialError::Storage);
    }
    match status {
        0 => Ok(None),
        1 if operation != Operation::Put => Ok(None),
        2 => Err(CredentialError::Unavailable),
        3 => Err(CredentialError::Storage),
        4 => Err(CredentialError::Ambiguous),
        5 => Err(CredentialError::WriteUnknown),
        _ => Err(CredentialError::Storage),
    }
}

#[cfg(target_os = "android")]
mod native {
    use super::{CredentialError, Operation, decode_reply};
    use crate::credential_native::{StoredCredential, decode_credential};
    use jni::{
        JavaVM,
        objects::{JByteArray, JClass, JObject, JValue},
    };
    use notryx_client_store::RegistrationBinding;
    use uuid::Uuid;

    fn uncertain(operation: Operation) -> CredentialError {
        if operation == Operation::Get {
            CredentialError::Unavailable
        } else {
            CredentialError::WriteUnknown
        }
    }

    fn call_java(
        operation: Operation,
        id: Uuid,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, CredentialError> {
        // Tao initializes and retains the application Context before starting Rust.
        // Borrow its global JNI reference without deleting it or exposing it to WebView.
        let context = ndk_context::android_context();
        let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }
            .map_err(|_| CredentialError::Unavailable)?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|_| CredentialError::Unavailable)?;
        let mut submitted = false;
        let result = env.with_local_frame(24, |env| -> jni::errors::Result<Vec<u8>> {
            let context = unsafe { JObject::from_raw(context.context().cast()) };
            let loader = env
                .call_method(&context, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
                .l()?;
            let class_name = env.new_string("org.notryx.m0.CredentialStore")?;
            let class = JClass::from(
                env.call_method(
                    &loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[JValue::Object(&class_name)],
                )?
                .l()?,
            );
            let id = env.new_string(id.to_string())?;
            let payload = env.byte_array_from_slice(&payload)?;
            let mut args = vec![JValue::Object(&context), JValue::Object(&id)];
            let (method, signature) = match operation {
                Operation::Get => ("get", "(Landroid/content/Context;Ljava/lang/String;)[B"),
                Operation::Put => ("put", "(Landroid/content/Context;Ljava/lang/String;[B)[B"),
                Operation::Delete => (
                    "delete",
                    "(Landroid/content/Context;Ljava/lang/String;[B)[B",
                ),
            };
            if operation != Operation::Get {
                args.push(JValue::Object(&payload));
            }
            submitted = true;
            let reply = JByteArray::from(
                env.call_static_method(&class, method, signature, &args)?
                    .l()?,
            );
            if env.get_array_length(&reply)? > 8193 {
                return Ok(Vec::new());
            }
            env.convert_byte_array(&reply)
        });
        if result.is_err() {
            // Java exception messages may contain provider details or credential material.
            let _ = env.exception_clear();
        }
        result.map_err(|_| {
            if submitted {
                uncertain(operation)
            } else {
                CredentialError::Unavailable
            }
        })
    }

    async fn invoke(
        operation: Operation,
        id: Uuid,
        payload: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, CredentialError> {
        let task = tokio::task::spawn_blocking(move || call_java(operation, id, payload));
        // A timeout cannot cancel a Keystore write. Keep pending state until a later read proves its result.
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .map_err(|_| uncertain(operation))?
            .map_err(|_| uncertain(operation))??;
        decode_reply(operation, reply)
    }

    pub(in crate::credential_native) async fn put(
        id: Uuid,
        credential: &StoredCredential,
    ) -> Result<(), CredentialError> {
        if decode_credential(credential.token.clone())?.binding != credential.binding {
            return Err(CredentialError::BindingMismatch);
        }
        invoke(Operation::Put, id, credential.token.clone())
            .await
            .map(|_| ())
    }

    pub(in crate::credential_native) async fn get(
        id: Uuid,
    ) -> Result<Vec<StoredCredential>, CredentialError> {
        match invoke(Operation::Get, id, Vec::new()).await? {
            Some(payload) => Ok(vec![decode_credential(payload)?]),
            None => Ok(Vec::new()),
        }
    }

    pub(in crate::credential_native) async fn delete(
        id: Uuid,
        binding: &RegistrationBinding,
    ) -> Result<(), CredentialError> {
        let Some(record) = get(id).await?.pop() else {
            return Ok(());
        };
        if &record.binding != binding {
            return Err(CredentialError::BindingMismatch);
        }
        invoke(Operation::Delete, id, record.token)
            .await
            .map(|_| ())
    }
}

#[cfg(target_os = "android")]
pub(super) use native::{delete, get, put};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_reply_returns_only_bounded_get_payloads() {
        assert_eq!(
            decode_reply(Operation::Get, vec![0, 7, 8]),
            Ok(Some(vec![7, 8]))
        );
        assert_eq!(decode_reply(Operation::Get, vec![1]), Ok(None));
        assert_eq!(decode_reply(Operation::Put, vec![0]), Ok(None));
        assert_eq!(decode_reply(Operation::Delete, vec![0]), Ok(None));
        assert_eq!(decode_reply(Operation::Delete, vec![1]), Ok(None));
        for (operation, reply) in [
            (Operation::Get, vec![0]),
            (Operation::Get, vec![0; 8194]),
            (Operation::Put, vec![0, 7]),
            (Operation::Put, vec![1]),
            (Operation::Get, vec![9]),
            (Operation::Get, vec![]),
        ] {
            assert_eq!(
                decode_reply(operation, reply),
                Err(CredentialError::Storage)
            );
        }
    }

    #[test]
    fn android_reply_never_treats_corruption_or_unknown_writes_as_session_only() {
        for operation in [Operation::Put, Operation::Get, Operation::Delete] {
            for (code, error) in [
                (2, CredentialError::Unavailable),
                (3, CredentialError::Storage),
                (4, CredentialError::Ambiguous),
                (5, CredentialError::WriteUnknown),
            ] {
                assert_eq!(decode_reply(operation, vec![code]), Err(error));
            }
            assert_eq!(
                decode_reply(operation, vec![2, 7]),
                Err(CredentialError::Storage)
            );
        }
    }
}
