package org.notryx.m0

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import java.util.UUID
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

object CredentialProbe {
    fun run(): Boolean {
        val alias = "notryx-m0-probe-${UUID.randomUUID()}"
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        try {
            val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
            generator.init(KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build())
            generator.generateKey()
            val original = "synthetic-m0-token".toByteArray(Charsets.UTF_8)
            val encrypt = Cipher.getInstance("AES/GCM/NoPadding")
            encrypt.init(Cipher.ENCRYPT_MODE, store.getKey(alias, null) as SecretKey)
            val ciphertext = encrypt.doFinal(original)
            val decrypt = Cipher.getInstance("AES/GCM/NoPadding")
            decrypt.init(Cipher.DECRYPT_MODE, store.getKey(alias, null) as SecretKey, GCMParameterSpec(128, encrypt.iv))
            return decrypt.doFinal(ciphertext).contentEquals(original)
        } finally {
            store.deleteEntry(alias)
        }
    }
}
