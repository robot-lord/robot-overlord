// The engine's platform keys: the relay identity (`Signers`) and the key
// that wraps everything the engine stores (`Vault`). Both are P-256 keys
// in the Secure Enclave, made on this phone, never extractable, readable
// after the first unlock (a background check runs while the phone is
// locked) and in no backup. The content key (X25519) can't live in the
// enclave; the engine keeps it in its sealed file, wrapped by the vault.
//
// Without an enclave (an Intel Mac's simulator; the Apple Silicon
// simulator passes the Mac's enclave through), a debug build falls back
// to software keys in the Keychain and says so on the status screen. A
// release build refuses to run without the enclave (Shell).

import CryptoKit
import Foundation
import Security

enum Keys {
    /// Keys are made in the Secure Enclave.
    static var hardware: Bool { SecureEnclave.isAvailable }

    private static let tagPrefix = "com.robot-lord.rlord.key."

    static func tag(_ label: String) -> Data { Data((tagPrefix + label).utf8) }

    /// Make a P-256 key under `label`, replacing any.
    static func create(_ label: String) throws -> SecKey {
        delete(label)
        var error: Unmanaged<CFError>?
        var privateAttrs: [String: Any] = [
            kSecAttrIsPermanent as String: true,
            kSecAttrApplicationTag as String: tag(label),
        ]
        var attrs: [String: Any] = [
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeySizeInBits as String: 256,
        ]
        if hardware {
            guard let access = SecAccessControlCreateWithFlags(
                nil, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly, .privateKeyUsage, &error)
            else { throw failure("access control", error) }
            privateAttrs[kSecAttrAccessControl as String] = access
            attrs[kSecAttrTokenID as String] = kSecAttrTokenIDSecureEnclave
        } else {
            privateAttrs[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        }
        attrs[kSecPrivateKeyAttrs as String] = privateAttrs
        guard let key = SecKeyCreateRandomKey(attrs as CFDictionary, &error) else {
            throw failure("create key", error)
        }
        return key
    }

    /// The private key under `label`, if there is one.
    static func find(_ label: String) -> SecKey? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassKey,
            kSecAttrApplicationTag as String: tag(label),
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecReturnRef as String: true,
        ]
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess, let item else {
            return nil
        }
        return (item as! SecKey)
    }

    static func delete(_ label: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassKey,
            kSecAttrApplicationTag as String: tag(label),
        ]
        SecItemDelete(query as CFDictionary)
    }

    /// The SEC1 uncompressed public key (65 bytes).
    static func publicKey(_ key: SecKey) throws -> Data {
        var error: Unmanaged<CFError>?
        guard let pub = SecKeyCopyPublicKey(key),
              let bytes = SecKeyCopyExternalRepresentation(pub, &error) as Data?
        else { throw failure("public key", error) }
        return bytes
    }

    static func failure(_ what: String, _ error: Unmanaged<CFError>?) -> FfiError {
        let detail = error.map { String(describing: $0.takeRetainedValue()) } ?? "unknown error"
        return FfiError.Failed(message: "\(what): \(detail)")
    }
}

/// The relay identity, one per paired host (the engine names them).
final class EnclaveSigners: Signers, @unchecked Sendable {
    func create(label: String) throws -> Data {
        try Keys.publicKey(try Keys.create("signer." + label))
    }

    func publicKey(label: String) throws -> Data {
        guard let key = Keys.find("signer." + label) else {
            throw FfiError.Failed(message: "no key \(label)")
        }
        return try Keys.publicKey(key)
    }

    /// ECDSA P-256 with SHA-256, as ASN.1 DER: the engine converts it.
    func sign(label: String, msg: Data) throws -> Data {
        guard let key = Keys.find("signer." + label) else {
            throw FfiError.Failed(message: "no key \(label)")
        }
        var error: Unmanaged<CFError>?
        guard let sig = SecKeyCreateSignature(
            key, .ecdsaSignatureMessageX962SHA256, msg as CFData, &error) as Data?
        else { throw Keys.failure("sign", error) }
        return sig
    }

    func delete(label: String) {
        Keys.delete("signer." + label)
    }

    func hardware() -> Bool { Keys.hardware }
}

/// Wraps the engine's files at rest with ECIES to a P-256 key in the
/// enclave (made on first use).
final class EnclaveVault: Vault, @unchecked Sendable {
    private static let label = "vault"
    private static let algorithm = SecKeyAlgorithm.eciesEncryptionCofactorVariableIVX963SHA256AESGCM
    private let lock = NSLock()

    private func key() throws -> SecKey {
        lock.lock()
        defer { lock.unlock() }
        if let key = Keys.find(Self.label) { return key }
        return try Keys.create(Self.label)
    }

    func seal(plain: Data) throws -> Data {
        let key = try key()
        guard let pub = SecKeyCopyPublicKey(key) else {
            throw FfiError.Failed(message: "vault: no public key")
        }
        var error: Unmanaged<CFError>?
        guard let sealed = SecKeyCreateEncryptedData(pub, Self.algorithm, plain as CFData, &error) as Data?
        else { throw Keys.failure("seal", error) }
        return sealed
    }

    func unseal(sealed: Data) throws -> Data {
        let key = try key()
        var error: Unmanaged<CFError>?
        guard let plain = SecKeyCreateDecryptedData(key, Self.algorithm, sealed as CFData, &error) as Data?
        else { throw Keys.failure("open", error) }
        return plain
    }

    /// Unpairing everything removes the vault key too.
    static func wipe() { Keys.delete(label) }
}
