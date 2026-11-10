package org.notryx.m0

import java.io.File
import java.io.IOException
import java.security.InvalidKeyException
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class CredentialFilesTest {
    @get:Rule val temporary = TemporaryFolder()
    private val id = "6ce5c093-ce26-41af-ab89-c68700efe83d"
    private val otherId = "d3f0171b-101b-4719-a09f-179b18958ee1"
    private val payload = "synthetic-token-for-jvm-test-only".toByteArray()
    private val secret = KeyGenerator.getInstance("AES").apply { init(256) }.generateKey()

    private fun store(directory: File, provider: (Boolean) -> SecretKey = { secret }, sync: () -> Unit = {}) =
        CredentialFiles(directory, provider, sync)

    private fun status(expected: Int, response: ByteArray) =
        assertArrayEquals(byteArrayOf(expected.toByte()), response)

    @Test fun recreationReadsCiphertextWithoutPlaintextFiles() {
        val directory = temporary.newFolder()
        status(0, store(directory).put(id, payload))
        status(0, store(directory).put(otherId, payload))
        assertArrayEquals(byteArrayOf(0) + payload, store(directory).get(id))
        val first = File(directory, id).readBytes()
        val second = File(directory, otherId).readBytes()
        assertEquals(1, first[0].toInt())
        assertEquals(payload.size + 1 + 12 + 16, first.size)
        assertFalse(first.copyOfRange(1, 13).contentEquals(second.copyOfRange(1, 13)))
        assertEquals(2, directory.listFiles()!!.size)
        directory.listFiles()!!.forEach {
            assertFalse(it.readBytes().toString(Charsets.ISO_8859_1).contains(payload.toString(Charsets.UTF_8)))
        }
    }

    @Test fun tamperingAndMovingCiphertextToAnotherIdAreRejectedAndPreserved() {
        val directory = temporary.newFolder()
        val files = store(directory)
        status(0, files.put(id, payload))
        val encrypted = File(directory, id).readBytes()
        File(directory, otherId).writeBytes(encrypted)
        status(3, files.get(otherId))
        encrypted[encrypted.lastIndex] = (encrypted.last().toInt() xor 1).toByte()
        File(directory, id).writeBytes(encrypted)
        status(3, files.get(id))
        status(3, files.delete(id, payload))
        assertArrayEquals(encrypted, File(directory, id).readBytes())
    }

    @Test fun duplicatePutDoesNotOverwriteOrRequestAKey() {
        val directory = temporary.newFolder()
        status(0, store(directory).put(id, payload))
        val before = File(directory, id).readBytes()
        val files = store(directory, { throw AssertionError("duplicate must not request key") })
        status(4, files.put(id, "replacement".toByteArray()))
        assertArrayEquals(before, File(directory, id).readBytes())
        assertArrayEquals(byteArrayOf(0) + payload, store(directory).get(id))
    }

    @Test fun unavailableKeyDoesNotWriteOrRegenerateOnReadAndDelete() {
        val directory = temporary.newFolder()
        status(0, store(directory).put(id, payload))
        val before = File(directory, id).readBytes()
        val requests = mutableListOf<Boolean>()
        val files = store(directory, { create ->
            requests.add(create)
            throw InvalidKeyException("synthetic unavailable key")
        })
        status(2, files.put(otherId, payload))
        status(2, files.get(id))
        status(2, files.delete(id, payload))
        assertEquals(listOf(true, false, false), requests)
        assertEquals(1, directory.listFiles()!!.size)
        assertArrayEquals(before, File(directory, id).readBytes())
    }

    @Test fun deleteRequiresMatchingPlaintextAndSyncsBeforeSuccess() {
        val directory = temporary.newFolder()
        var syncs = 0
        val files = store(directory, sync = { syncs++ })
        status(0, files.put(id, payload))
        val before = File(directory, id).readBytes()
        status(4, files.delete(id, "different".toByteArray()))
        assertArrayEquals(before, File(directory, id).readBytes())
        assertEquals(1, syncs)
        status(0, files.delete(id, payload))
        assertEquals(2, syncs)
        status(1, files.get(id))
        status(1, files.delete(id, payload))
    }

    @Test fun syncFailureReturnsUnknownAndDoesNotRemoveCommittedCiphertext() {
        val directory = temporary.newFolder()
        val files = store(directory, sync = { throw IOException("synthetic fsync failure") })
        status(5, files.put(id, payload))
        assertArrayEquals(byteArrayOf(0) + payload, store(directory).get(id))
        status(4, files.put(id, payload))
        status(5, files.delete(id, payload))
        status(1, store(directory).get(id))
    }

    @Test fun writeFailureReturnsUnknownWithoutChangingOtherFiles() {
        val blockedDirectory = temporary.newFile()
        blockedDirectory.writeBytes(payload)
        status(5, store(blockedDirectory).put(id, payload))
        assertArrayEquals(payload, blockedDirectory.readBytes())
    }

    @Test fun invalidIdsOversizedPayloadsAndInvalidEnvelopesAreRejected() {
        val directory = temporary.newFolder()
        val files = store(directory)
        for (invalid in listOf("../escape", id.uppercase(), "1-1-1-1-1")) {
            status(3, files.put(invalid, payload))
            status(3, files.get(invalid))
            status(3, files.delete(invalid, payload))
        }
        status(3, files.put(id, ByteArray(8193)))
        assertTrue(directory.listFiles()!!.isEmpty())
        status(1, store(directory, { throw AssertionError("missing must not request key") }).get(id))
        status(0, files.put(id, ByteArray(8192) { 42 }))
        assertArrayEquals(byteArrayOf(0) + ByteArray(8192) { 42 }, files.get(id))
        File(directory, id).writeBytes(byteArrayOf(1, 2, 3))
        status(3, files.get(id))
        File(directory, id).writeBytes(ByteArray(9000))
        status(3, files.get(id))
    }
}
