import Foundation
import Security

@_silgen_name("novakey_llm_on_result")
func novakey_llm_on_result(
    _ controllerId: UInt64, _ generation: UInt64,
    _ status: Int32, _ textPtr: UnsafePointer<UInt8>?, _ textLen: Int)

private let keychainService = "me.sijis.inputmethod.NovakeyR.cloudflare"
private let rankInstructions = "読みに対して最も自然なかな漢字変換候補を選べ"
private let requestTimeout: TimeInterval = 3
private let debounceMilliseconds = 250

private struct Credentials {
    let apiToken: String
    let accountId: String
}

private func keychainString(account: String) -> String? {
    let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: keychainService,
        kSecAttrAccount as String: account,
        kSecReturnData as String: true,
        kSecMatchLimit as String: kSecMatchLimitOne,
    ]
    var item: CFTypeRef?
    guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
          let data = item as? Data,
          let value = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
          !value.isEmpty else { return nil }
    return value
}

// グローバル let は初回アクセス時に一度だけ評価される(lazy once)。
// Keychain を後から登録した場合は IME の再起動が必要。
private let credentials: Credentials? = {
    guard let token = keychainString(account: "api-token"),
          let account = keychainString(account: "account-id") else { return nil }
    return Credentials(apiToken: token, accountId: account)
}()

private let session: URLSession = {
    let config = URLSessionConfiguration.ephemeral
    config.timeoutIntervalForRequest = requestTimeout
    config.timeoutIntervalForResource = requestTimeout
    return URLSession(configuration: config)
}()

private struct ClefResponse: Decodable {
    struct Result: Decodable {
        struct Answers: Decodable {
            struct Rank: Decodable { let choice: String }
            let rank: Rank
        }
        let answers: Answers
    }
    let success: Bool
    let result: Result?
}

private enum RerankFailure: Error {
    case invalidResponse
}

private struct RerankKey: Equatable {
    let context: String
    let reading: String
    let candidates: [String]
}

private var latestGenerations: [UInt64: UInt64] = [:]
private var inFlightTasks: [UInt64: Task<Void, Never>] = [:]
// 送信を始めた(または結果を配信した)世代。urgent の重複送信を防ぐ。
private var sentGenerations: [UInt64: UInt64] = [:]
// 直近 1 件の成功結果。同一リクエストは HTTP を送らずこれを返す。
private var lastResult: (key: RerankKey, index: Int)?
private var requestCount = 0

@MainActor
private func beginSend(_ id: UInt64, _ gen: UInt64) -> Bool {
    guard latestGenerations[id] == gen, !Task.isCancelled else { return false }
    sentGenerations[id] = gen
    requestCount += 1
    NSLog("%@", "Clef requests sent: \(requestCount)")
    return true
}

@MainActor
private func storeResult(_ key: RerankKey, _ index: Int) {
    lastResult = (key, index)
}

@MainActor
private func registerLatest(_ id: UInt64, _ gen: UInt64) {
    latestGenerations[id] = gen
    inFlightTasks[id]?.cancel()
}

@MainActor
private func isLatest(_ id: UInt64, _ gen: UInt64) -> Bool {
    latestGenerations[id] == gen
}

@MainActor
private func trackTask(_ id: UInt64, _ gen: UInt64, _ task: Task<Void, Never>) {
    if latestGenerations[id] == gen {
        inFlightTasks[id] = task
    }
}

@MainActor
private func clearInFlightTask(_ id: UInt64, _ gen: UInt64) {
    if latestGenerations[id] == gen {
        inFlightTasks[id] = nil
    }
}

@_cdecl("novakey_llm_availability")
public func novakey_llm_availability() -> Int32 {
    credentials == nil ? 4 : 0
}

private func deliver(_ id: UInt64, _ gen: UInt64, _ status: Int32, _ text: String) {
    DispatchQueue.main.async {
        guard latestGenerations[id] == gen else { return }
        var bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer { buf in
            novakey_llm_on_result(id, gen, status, buf.baseAddress, buf.count)
        }
    }
}

private func buildRequest(_ creds: Credentials, context: String, reading: String, candidates: [String]) throws -> URLRequest {
    var criteria: [String: String] = [:]
    for (i, c) in candidates.enumerated() { criteria[String(i)] = c }
    var state: [String: String] = ["reading": reading]
    if !context.isEmpty { state["context"] = context }
    let body: [String: Any] = [
        "model": "clef-flash",
        "state": state,
        "questions": [
            "rank": [
                "type": "choice",
                "instructions": rankInstructions,
                "criteria": criteria,
            ] as [String: Any]
        ],
    ]
    let url = URL(string: "https://api.cloudflare.com/client/v4/accounts/\(creds.accountId)/ai/run/@cf/cloudflare/clef-flash")!
    var request = URLRequest(url: url, timeoutInterval: requestTimeout)
    request.httpMethod = "POST"
    request.setValue("Bearer \(creds.apiToken)", forHTTPHeaderField: "Authorization")
    request.setValue("application/json", forHTTPHeaderField: "Content-Type")
    request.httpBody = try JSONSerialization.data(withJSONObject: body)
    return request
}

private func fetchChoice(_ creds: Credentials, context: String, reading: String, candidates: [String]) async throws -> Int {
    let request = try buildRequest(creds, context: context, reading: reading, candidates: candidates)
    let (data, response) = try await session.data(for: request)
    guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
        throw RerankFailure.invalidResponse
    }
    let decoded = try JSONDecoder().decode(ClefResponse.self, from: data)
    guard decoded.success,
          let choice = decoded.result?.answers.rank.choice,
          let index = Int(choice),
          String(index) == choice,
          index >= 0, index < candidates.count else {
        throw RerankFailure.invalidResponse
    }
    return index
}

@_cdecl("novakey_llm_rerank")
public func novakey_llm_rerank(
    _ controllerId: UInt64, _ generation: UInt64,
    _ readingPtr: UnsafePointer<UInt8>, _ readingLen: Int,
    _ candsPtr: UnsafePointer<UInt8>, _ candsLen: Int,
    _ ctxPtr: UnsafePointer<UInt8>?, _ ctxLen: Int,
    _ urgent: Bool
) {
    let context = ctxPtr.map { String(decoding: UnsafeBufferPointer(start: $0, count: ctxLen), as: UTF8.self) } ?? ""
    let reading = String(decoding: UnsafeBufferPointer(start: readingPtr, count: readingLen), as: UTF8.self)
    let joined = String(decoding: UnsafeBufferPointer(start: candsPtr, count: candsLen), as: UTF8.self)
    let candidates = joined.split(separator: "\u{1F}", omittingEmptySubsequences: false).map(String.init)
    guard candidates.count >= 2, let creds = credentials else { return }
    let key = RerankKey(context: context, reading: reading, candidates: candidates)
    DispatchQueue.main.async {
        if let cached = lastResult, cached.key == key {
            registerLatest(controllerId, generation)
            sentGenerations[controllerId] = generation
            deliver(controllerId, generation, 0, String(cached.index))
            return
        }
        if urgent, sentGenerations[controllerId] == generation, latestGenerations[controllerId] == generation {
            return
        }
        registerLatest(controllerId, generation)
        let delay = urgent ? 0 : debounceMilliseconds
        let task = Task {
            if delay > 0 {
                do { try await Task.sleep(for: .milliseconds(delay)) } catch { return }
            }
            guard await beginSend(controllerId, generation) else { return }
            do {
                let index = try await fetchChoice(creds, context: context, reading: reading, candidates: candidates)
                await storeResult(key, index)
                guard await isLatest(controllerId, generation), !Task.isCancelled else { return }
                deliver(controllerId, generation, 0, String(index))
            } catch {
                guard await isLatest(controllerId, generation), !Task.isCancelled else { return }
                deliver(controllerId, generation, 1, "Clef rerank failed")
            }
            await clearInFlightTask(controllerId, generation)
        }
        trackTask(controllerId, generation, task)
    }
}
