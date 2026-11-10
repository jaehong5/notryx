package org.notryx.m0

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.system.Os
import android.system.OsConstants
import androidx.annotation.Keep
import java.io.File
import java.security.InvalidKeyException
import java.security.KeyStore
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey

@Keep
object CredentialStore {
    private const val ALIAS = "org.notryx.credential-key.v1"

    @JvmStatic
    @Synchronized
    fun put(context: Context, id: String, payload: ByteArray): ByteArray = try {
        files(context).put(id, payload)
    } catch (_: Exception) {
        byteArrayOf(3)
    }

    @JvmStatic
    @Synchronized
    fun get(context: Context, id: String): ByteArray = try {
        files(context).get(id)
    } catch (_: Exception) {
        byteArrayOf(3)
    }

    @JvmStatic
    @Synchronized
    fun delete(context: Context, id: String, expected: ByteArray): ByteArray = try {
        files(context).delete(id, expected)
    } catch (_: Exception) {
        byteArrayOf(3)
    }

    private fun files(context: Context): CredentialFiles {
        val parent = context.applicationContext.noBackupFilesDir
        val directory = File(parent, "notryx-credentials-v1")
        return CredentialFiles(directory, ::key) {
            sync(directory)
            sync(parent)
        }
    }

    private fun key(create: Boolean): SecretKey = try {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val existing = store.getKey(ALIAS, null)
        if (existing is SecretKey) {
            existing
        } else {
            if (existing != null || store.containsAlias(ALIAS) || !create) throw InvalidKeyException()
            KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").run {
                init(KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                    .setKeySize(256)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setRandomizedEncryptionRequired(true)
                    .build())
                generateKey()
            }
        }
    } catch (_: Exception) {
        throw InvalidKeyException()
    }

    private fun sync(directory: File) {
        val fd = Os.open(directory.path, OsConstants.O_RDONLY, 0)
        try {
            Os.fsync(fd)
        } finally {
            Os.close(fd)
        }
    }
}
