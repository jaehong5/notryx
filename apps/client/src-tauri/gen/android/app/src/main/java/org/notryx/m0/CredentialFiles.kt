package org.notryx.m0

import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.security.GeneralSecurityException
import java.security.MessageDigest
import java.security.ProviderException
import java.util.UUID
import javax.crypto.BadPaddingException
import javax.crypto.Cipher
import javax.crypto.IllegalBlockSizeException
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

internal class CredentialFiles(
    private val directory: File,
    private val key: (create: Boolean) -> SecretKey,
    private val syncDirectory: () -> Unit,
) {
    fun put(id: String, payload: ByteArray): ByteArray {
        if (!validId(id) || payload.size > MAX_PLAINTEXT) return status(STORAGE)
        var writing = false
        var temporary: File? = null
        try {
            val destination = File(directory, id)
            if (destination.exists()) return status(MISMATCH)
            val encrypted = try {
                val cipher = Cipher.getInstance("AES/GCM/NoPadding")
                // Android Keystore requires the provider to generate encryption IVs.
                cipher.init(Cipher.ENCRYPT_MODE, key(true))
                cipher.updateAAD(aad(id))
                val ciphertext = cipher.doFinal(payload)
                if (cipher.iv.size != IV_BYTES || ciphertext.size != payload.size + TAG_BYTES) {
                    return status(UNAVAILABLE)
                }
                byteArrayOf(VERSION) + cipher.iv + ciphertext
            } catch (_: GeneralSecurityException) {
                return status(UNAVAILABLE)
            } catch (_: ProviderException) {
                return status(UNAVAILABLE)
            }
            writing = true
            if (!directory.isDirectory && !directory.mkdirs()) throw IOException()
            temporary = File.createTempFile(".credential-", ".tmp", directory)
            FileOutputStream(temporary).use {
                it.write(encrypted)
                it.fd.sync()
            }
            if (destination.exists()) return status(MISMATCH)
            if (!temporary.renameTo(destination)) throw IOException()
            syncDirectory()
            return status(OK)
        } catch (_: Exception) {
            return status(if (writing) UNKNOWN else STORAGE)
        } finally {
            try {
                temporary?.delete()
            } catch (_: Exception) {
                // An orphan here contains ciphertext only; never delete the destination.
            }
        }
    }

    fun get(id: String): ByteArray {
        if (!validId(id)) return status(STORAGE)
        try {
            if (!directory.exists()) return status(MISSING)
            if (!directory.isDirectory) return status(STORAGE)
            val source = File(directory, id)
            if (!source.exists()) return status(MISSING)
            if (!source.isFile) return status(STORAGE)
            val bytes = FileInputStream(source).use { input ->
                val buffer = ByteArray(MAX_ENVELOPE + 1)
                var count = 0
                while (count < buffer.size) {
                    val read = input.read(buffer, count, buffer.size - count)
                    if (read < 0) break
                    count += read
                }
                buffer.copyOf(count)
            }
            if (bytes.size !in (1 + IV_BYTES + TAG_BYTES)..MAX_ENVELOPE || bytes[0] != VERSION) {
                return status(STORAGE)
            }
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.DECRYPT_MODE, key(false), GCMParameterSpec(128, bytes, 1, IV_BYTES))
            cipher.updateAAD(aad(id))
            return status(OK) + cipher.doFinal(bytes, 1 + IV_BYTES, bytes.size - 1 - IV_BYTES)
        } catch (_: BadPaddingException) {
            return status(STORAGE)
        } catch (_: IllegalBlockSizeException) {
            return status(STORAGE)
        } catch (_: GeneralSecurityException) {
            return status(UNAVAILABLE)
        } catch (_: ProviderException) {
            return status(UNAVAILABLE)
        } catch (_: Exception) {
            return status(STORAGE)
        }
    }

    fun delete(id: String, expected: ByteArray): ByteArray {
        if (expected.size > MAX_PLAINTEXT) return status(STORAGE)
        val stored = get(id)
        if (stored[0] != OK) return stored
        if (!MessageDigest.isEqual(stored.copyOfRange(1, stored.size), expected)) return status(MISMATCH)
        return try {
            if (!File(directory, id).delete()) return status(UNKNOWN)
            syncDirectory()
            status(OK)
        } catch (_: Exception) {
            status(UNKNOWN)
        }
    }

    private fun validId(id: String): Boolean = try {
        UUID.fromString(id).toString() == id
    } catch (_: IllegalArgumentException) {
        false
    }

    private fun aad(id: String) = "notryx-credential-v1:$id".toByteArray(Charsets.UTF_8)
    private fun status(value: Byte) = byteArrayOf(value)

    private companion object {
        const val OK: Byte = 0
        const val MISSING: Byte = 1
        const val UNAVAILABLE: Byte = 2
        const val STORAGE: Byte = 3
        const val MISMATCH: Byte = 4
        const val UNKNOWN: Byte = 5
        const val VERSION: Byte = 1
        const val IV_BYTES = 12
        const val TAG_BYTES = 16
        const val MAX_PLAINTEXT = 8192
        const val MAX_ENVELOPE = 1 + IV_BYTES + MAX_PLAINTEXT + TAG_BYTES
    }
}
