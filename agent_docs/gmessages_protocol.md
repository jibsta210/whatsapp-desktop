# Google Messages Web Protocol (mautrix-gmessages) — Rust Porting Guide

## Executive Summary

This document describes the Google Messages web protocol as implemented by mautrix-gmessages (https://github.com/mautrix/gmessages), a Matrix-to-Google-Messages puppeting bridge. The protocol uses protobuf (PBLite variant) for serialization, ECDSA + UKEY2 for pairing, and AES-CTR/AES-GCM for encryption. The bridge connects a desktop web client to a paired Android phone, relaying all messages through Google's relay servers.

**Key Constraint:** The paired Android phone must be online and available for the desktop client to function. This is a relay protocol, not a cloud-sync protocol.

---

## 1. Connection & Pairing

### 1.1 Desktop-to-Relay Connection Flow

The desktop client connects to Google's relay infrastructure via REST endpoints:

**Key Endpoints** (`pkg/libgm/util/paths.go`):
- **Base URL:** `https://messages.google.com`
- **Authentication:** `https://messages.google.com/web/authentication`
- **Relay RPC (Standard):** `https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1.{Messaging,Pairing,Registration}`
- **Relay RPC (Google Account):** `https://instantmessaging-pa.clients6.google.com/$rpc/...` (for Gaia-authenticated users)

**HTTP Transport Details** (`pkg/libgm/http.go`):
- **Content-Type:** `application/json+protobuf` (PBLite encoding) or `application/x-protobuf` (raw protobuf)
- **Authentication Headers:** `SAPISID` cookie is hashed using SHA1 to create `Authorization: SAPISIDHASH timestamp_hash` header (line 63-67)
- **Timeout:** 2 minutes for regular requests, 30 minutes for long-polling
- **TLS Handshake Timeout:** 10 seconds
- **Response Header Timeout:** 20 seconds

### 1.2 QR Code Pairing Flow (Phone Relay Method)

For non-Google accounts, users scan a QR code on their phone. The protocol is UKEY2-based with emoji verification.

**Flow:**

1. **Client Init** → Server (`pkg/libgm/pair_google.go:159-179`)
   - Generate ECDSA P-256 key pair
   - Create `Ukey2ClientInit` with:
     - Version: 1
     - Random: 32 bytes
     - CipherCommitments: `P256_SHA512`
     - NextProtocol: `"AES_256_CBC-HMAC_SHA256"`
   - Wrap in `Ukey2Message` with type `CLIENT_INIT`
   - Compute SHA512(finish_payload) as key commitment

2. **Server Init Response** → Extract emoji
   - Phone responds with `Ukey2ServerInit` containing server P-256 public key
   - Perform ECDH with client's P-256 private key
   - Compute shared secret: SHA256(ECDH output)
   - Derive authentication key via HKDF:
     - `authInfo = client_init_payload || server_init_response`
     - `auth_key = HKDF-SHA256(shared_secret, "UKEY2 v1 auth", authInfo, 32 bytes)`
     - `next_key = HKDF-SHA256(shared_secret, "UKEY2 v1 next", authInfo, 32 bytes)`
   - Extract emoji from auth_key first 4 bytes (big-endian uint32 % emoji_list_size)
   - Two emoji versions supported (v0: 280 emojis, v1: 280 + new - removed)

3. **User Confirms Emoji on Phone**
   - Phone displays matching emoji to user
   - User verifies both sides match

4. **Client Finish** → Server
   - Send `Ukey2ClientFinished` with client P-256 public key
   - Wrapped in `Ukey2Message` with type `CLIENT_FINISH`

5. **Exchange Complete**
   - Both sides now share `next_key` for subsequent communication
   - `next_key` becomes the session encryption key for all subsequent RPC messages

**QR Code Encoding** (`pkg/libgm/pair_google.go:207`):
- Base URL: `https://support.google.com/messages/?p=web_computer#?c=`
- Appended data is base64url-encoded pairing request container

**Timeout:** 20 seconds for initial pairing setup (`GaiaInitTimeout`, line 296)

### 1.3 Google Account Pairing (Gaia Flow)

For Google accounts, users authenticate via their Google credentials.

**Flow** (`pkg/libgm/pair_google.go:55-104`):

1. **SignInGaia Initial Request**
   - `SignInGaiaRequest` with:
     - AuthMessage (requestID, network="GDitto", configVersion, tachyonAuthToken)
     - Inner.DeviceID: `messages-web-{uuid}`
   - Sent to `https://instantmessaging-pa.clients6.google.com/$rpc/google.internal.communications.instantmessaging.v1.Registration/SignInGaia`

2. **SignInGaia Get Token**
   - Send ECDSA refresh key (PKIX-encoded public key) in `Inner.SomeData`
   - Response contains `TokenData` with `tachyonAuthToken` and TTL
   - Extract mobile and browser Device objects
   - Cache both devices

3. **Token Refresh**
   - Token valid for TTL seconds (default buffer: 1 hour)
   - Use `RegisterRefresh` RPC with signed request (refresh key signs the request)
   - Signature computed over unix timestamp

**Key Persistence** (`pkg/libgm/client.go:27-48` — `AuthData` struct):
- `RequestCrypto`: AES-CTR helper for request/response encryption (key + HMAC key)
- `RefreshKey`: ECDSA private key for signing token refresh requests
- `Browser`, `Mobile`: Device identity objects
- `TachyonAuthToken`: JWT token for server authentication
- `TachyonExpiry`, `TachyonTTL`: Token lifetime tracking
- `SessionID`, `DestRegID`, `PairingID`: UUIDs for session tracking
- `Cookies`: Google account session cookies

### 1.4 Session Resumption

**Long Polling** (`pkg/libgm/longpoll.go`):
- WebSocket not used; instead HTTP long-polling on `ReceiveMessages` endpoint
- Client sends HTTP POST, server holds connection and streams `LongPollingPayload` messages
- Timeout: 30 minutes (via `lphttp` client)
- Payload contains:
  - `data`: Actual `IncomingRPCMessage`
  - `heartbeat`: Empty array (keep-alive)
  - `ack`: Start message count
  - `startRead`: Unused

**Ditto Ping Mechanism** (`pkg/libgm/longpoll.go:33-60`):
- Phone must receive "ditto" ping (NOTIFY_DITTO_ACTIVITY RPC) every ~1 minute
- If phone doesn't respond within 10-60 seconds, bridge signals `PhoneNotResponding`
- Backoff pinging with exponential intervals (1m → 2m → 4m → ... → 64m)
- If 3+ pings fail before notification sent, user is alerted

**Reconnection:**
- SessionID reset via `GetUpdates` RPC with fresh UUID
- Last received message acknowledgments prevent re-delivery
- Bridge deduplicates based on message ID hash (8-item ring buffer, line 130-137 in client.go)

---

## 2. Wire Format

### 2.1 Message Serialization

**PBLite Encoding:**
- Hybrid JSON + Protobuf format used for most RPC payloads
- Implemented in `go.mau.fi/util/pblite` package
- Binary fields marked with `[(pblite.pblite_binary) = true]` in proto definitions
- Both request and response can use either PBLite or raw protobuf depending on `Content-Type` header

**RPC Request Structure** (`pkg/libgm/gmproto/rpc.proto`):
```
OutgoingRPCMessage:
  - mobile: Device (source phone)
  - data: Data
    - requestID: string (UUID)
    - bugleRoute: BugleRoute
    - messageData: bytes (OutgoingRPCData encoded)
    - messageTypeData: Type
  - auth: Auth
    - requestID: string (same as data.requestID)
    - tachyonAuthToken: bytes
    - configVersion: ConfigVersion
  - TTL: int64 (milliseconds)
  - destRegistrationIDs: repeated string (destination device IDs)
```

**RPC Response Structure** (`pkg/libgm/gmproto/rpc.proto:21-45`):
```
IncomingRPCMessage:
  - responseID: string (UUID)
  - bugleRoute: BugleRoute (enum: DataEvent=19, PairEvent=14, GaiaEvent=7)
  - startExecute: uint64 (server timestamp)
  - messageType: MessageType (BUGLE_MESSAGE=2, GAIA_1=3, BUGLE_ANNOTATION=16, GAIA_2=20)
  - finishExecute: uint64
  - microsecondsTaken: uint64
  - mobile, browser: Device
  - messageData: bytes (depends on bugleRoute)
  - signatureID: string
  - timestamp: string (RFC3339 or unix?)
  - gdittoSource.deviceID: int32
```

**Encrypted Message Data** (`pkg/libgm/gmproto/rpc.proto:47-57`):
```
RPCMessageData:
  - sessionID: string (RequestID of original request)
  - timestamp: int64
  - action: ActionType (enum with 50+ RPC types)
  - unencryptedData: bytes (plain proto data)
  - encryptedData: bytes (AES-CTR encrypted)
  - encryptedData2: bytes (AES-CTR encrypted, alternative field)
  - bool1, bool2, bool3: unknown flags
```

### 2.2 Key Message Types

**Core Protobufs** (`pkg/libgm/gmproto/`):

| File | Key Messages | Purpose |
|------|--------------|---------|
| `conversations.proto` | `Message`, `Conversation`, `Participant`, `Contact`, `ReactionEntry` | Message + chat state |
| `authentication.proto` | `Device`, `PairedData`, `TokenData`, `Ukey2Message` | Auth & pairing |
| `events.proto` | `UpdateEvents`, `ConversationEvent`, `MessageEvent`, `TypingEvent`, `UserAlertEvent` | Push events from phone |
| `rpc.proto` | `OutgoingRPCMessage`, `IncomingRPCMessage`, `OutgoingRPCData` | RPC framing |
| `util.proto` | `EmptyArr` | Marker for empty arrays |

**Message Type Identifiers** (`pkg/libgm/gmproto/conversations.proto:38-55`):
```
Message:
  - messageID: string
  - msgType: MsgType (SMS, MMS, RCS, etc.)
  - messageStatus: MessageStatus (SEND_IN_PROGRESS, SENT, DELIVERED, READ, FAILED)
  - timestamp: int64 (milliseconds since epoch)
  - conversationID: string
  - participantID: string (sender)
  - messageInfo: repeated MessageInfo (contains MessageContent or MediaContent)
  - type: int64 (1=SMS, 2=downloaded MMS, 3=undownloaded MMS, 4=RCS)
  - reactions: repeated ReactionEntry
  - replyMessage: ReplyMessage (for message threads)
  - subject: string (MMS subject line)
```

**Reaction Format** (`pkg/libgm/gmproto/conversations.proto:57-77`):
```
ReactionEntry:
  - data: ReactionData
    - unicode: string (emoji or custom UUID)
    - type: EmojiType (LIKE=1, LOVE=2, LAUGH=3, ..., CUSTOM=8)
    - customEmoji: CustomEmojiData (for custom reactions)
  - participantIDs: repeated string (who reacted)
```

### 2.3 RPC Actions (50+ types)

Commonly used ActionType enum values (`pkg/libgm/gmproto/rpc.proto:117-168`):

- **LIST_CONVERSATIONS (1):** → `ListConversationsResponse`
- **LIST_MESSAGES (2):** → `ListMessagesResponse`
- **SEND_MESSAGE (3):** → `SendMessageResponse`
- **MESSAGE_UPDATES (4):** Event stream only (incoming)
- **CONVERSATION_UPDATES (7):** Event stream only
- **NOTIFY_DITTO_ACTIVITY (22):** Ping to keep phone alive
- **MESSAGE_READ (10):** → no response (ack)
- **TYPING_UPDATES (12):** → no response (ack)
- **SEND_REACTION (38):** → `SendReactionResponse`
- **DELETE_MESSAGE (23):** → `DeleteMessageResponse`
- **GET_UPDATES (16):** → `UpdateEvents` (used for session reset)
- **CREATE_GAIA_PAIRING_CLIENT_INIT (44):** → `GaiaPairingResponseContainer`
- **CREATE_GAIA_PAIRING_CLIENT_FINISHED (45):** → `GaiaPairingResponseContainer`

### 2.4 Decryption & Encoding

**Outgoing Messages** (`pkg/libgm/session_handler.go:189-250`):
1. Wrap user data proto in `OutgoingRPCData` with action type
2. Encrypt proto bytes with `RequestCrypto.Encrypt()` (AES-CTR + HMAC-SHA256)
3. Embed encrypted bytes in `RPCMessageData.encryptedData`
4. Encode full message as PBLite
5. POST to SendMessage endpoint with Tachyon auth token

**Incoming Messages** (`pkg/libgm/event_handler.go:49-124`):
1. Long-poll receives `LongPollingPayload` with `IncomingRPCMessage`
2. Route on `bugleRoute`:
   - **DataEvent (19):** Unmarshal `RPCMessageData`, decrypt `encryptedData` with `RequestCrypto.Decrypt()`
   - **PairEvent (14):** Unmarshal `RPCPairData` (contains `PairedData` or `RevokePairData`)
   - **GaiaEvent (7):** Unmarshal `RPCGaiaData` (Gaia-specific auth data)
3. Lookup expected response proto type from `responseType` map (line 29-47)
4. Unmarshal decrypted bytes into response message

---

## 3. Encryption

### 3.1 Desktop ↔ Phone Relay Channel Encryption

**AES-CTR Mode** (`pkg/libgm/crypto/aesctr.go`):
- All RPC data encrypted with AES-CTR
- Key and HMAC key each 32 bytes (256 bits), generated during pairing
- **Encryption:** IV (16 bytes random) || cipher || HMAC-SHA256(IV || cipher)
- **Decryption:** Extract HMAC, verify, extract IV, decrypt

**Shared Secret Derivation** (UKEY2 pairing):
- `shared_secret = SHA256(ECDH(client_private, server_public))`
- `HKDF-SHA256(shared_secret, "UKEY2 v1 next", authInfo, 32 bytes)` = next_key (used for AES-CTR)

**For Gaia (Google Account):**
- Initial keys exchanged via Google's SignInGaia RPC
- Same AES-CTR encryption applied to all subsequent messages

### 3.2 RCS E2EE Encryption (Signal Protocol)

**Not handled by mautrix-gmessages at the relay level.** The phone handles Signal-protocol encryption/decryption internally. The desktop bridge:
- Receives plaintext RCS message bodies (after phone decrypts)
- Sends RCS message bodies as plaintext; phone encrypts before sending to RCS network
- Does NOT access Signal protocol keys or perform Signal operations

**Implication for Rust port:** No dependency on libsignal or Signal protocol libraries needed.

### 3.3 Media Encryption

**Media Upload** (`pkg/libgm/media.go:91-121`):
1. Generate random 32-byte decryption key
2. Create AES-GCM cipher with key
3. Encrypt media bytes with AES-GCM
4. POST encrypted bytes to upload endpoint
5. Receive mediaID from server
6. Store mediaID + decryption key together in `MediaContent` proto

**Media Download** (not shown in excerpts, but implied):
- Fetch by mediaID from server
- Decrypt with stored decryption key using AES-GCM
- AES-GCM prefixes each encrypted chunk with nonce

**Key Sizes:**
- AES-GCM: 32-byte key (256 bits)
- Chunk overhead: 12-byte nonce + 16-byte GCM tag per chunk
- Chunk size: 32KB (65536 - 28 overhead)

---

## 4. Message Types

### 4.1 Incoming Messages (from phone)

**Message Arrival Flow:**
```
Long-poll ReceiveMessages
  ↓
LongPollingPayload.data (IncomingRPCMessage)
  ↓
bugleRoute=DataEvent → RPCMessageData
  ↓
action=MESSAGE_UPDATES → decrypt → UpdateEvents → MessageEvent
  ↓
MessageEvent.data = repeated Message
```

**Message Discriminators:**
- `type` field: 1=SMS, 2=MMS (downloaded), 3=MMS (undownloaded), 4=RCS
- `msgType` enum: SMS, MMS, RCS, etc. (exact enum in proto)
- `messageInfo`: repeated fields containing either `MessageContent` or `MediaContent`

**SMS Example:**
```proto
Message {
  messageID: "sms_123"
  type: 1  // SMS
  msgType: SMS
  conversationID: "+1234567890"
  timestamp: 1704067200000
  participantID: "+1234567890"
  messageInfo: [{
    messageContent: {text: "Hello world"}
  }]
  messageStatus: DELIVERED
}
```

**MMS Example:**
```proto
Message {
  messageID: "mms_456"
  type: 2  // MMS downloaded
  conversationID: "+1234567890"
  timestamp: 1704067200000
  messageInfo: [{
    mediaContent: {
      format: IMAGE_JPEG
      mediaID: "media_xyz"
      mediaName: "photo.jpg"
      size: 102400
      decryptionKey: [32 bytes]
    }
  }]
}
```

**RCS Example:**
```proto
Message {
  messageID: "rcs_789"
  type: 4  // RCS
  conversationID: "rcs_group_abc"
  timestamp: 1704067200000
  messageInfo: [{
    messageContent: {text: "RCS message"}
  }]
  messageStatus: DELIVERED
}
```

### 4.2 Sending Messages

**SendMessage RPC** (`pkg/libgm/methods.go:79-81`):
```go
c.SendMessage(payload *gmproto.SendMessageRequest)
  → ActionType_SEND_MESSAGE
  → returns SendMessageResponse
```

**SendMessageRequest Structure:**
- `conversationID`: Target chat
- `messageContent`: Text message bytes (proto-encoded `MessageContent`)
- `messageParts`: repeated `MessagePart` (for multi-part messages)
- `simPayload`: SIM slot info (for dual-SIM phones)
- `dryRun`: bool (test without sending)

**MediaContent Attachment:**
- Must be uploaded first (see Media Upload section)
- Include `MediaContent` object with mediaID + decryption key in message
- Mime type determined by first part of mime string (image/, video/, etc.)

**Reaction Sending** (`pkg/libgm/methods.go:101-104`):
```
SendReactionRequest {
  messageID: string
  conversationID: string
  reaction: ReactionData {
    unicode: string (emoji)
    type: EmojiType (LIKE, LOVE, LAUGH, etc.)
  }
}
```

**Read Receipt** (`pkg/libgm/methods.go:113-119`):
```
MessageReadRequest {
  conversationID: string
  messageID: string
}
→ ActionType_MESSAGE_READ
→ no response (sent as fire-and-forget)
```

**Typing Indicator** (`pkg/libgm/methods.go:121-129`):
```
TypingUpdateRequest {
  conversationID: string
  typing: bool (true = start, false = stop)
  simPayload: SIM info
}
→ ActionType_TYPING_UPDATES
→ no response (sent as fire-and-forget)
```

### 4.3 Group Chats

**Group Identification:**
- `conversationID` for groups is typically a UUID or group ID string (not a phone number)
- Participants extracted from `Conversation.participants` (array of `Participant` objects)
- Each `Participant` has `participantID` (phone or email) + `name` + `avatarHexColor`

**Group Operations:**
- **Leave Group:** `UpdateConversation` with action=LEAVE_RCS_GROUP
- **Add Participant:** `AddParticipantToRcsGroup` RPC (for RCS groups)
- **Change Group Name:** `UpdateConversation` with action=UPDATE_CONVERSATION
- **Change Participant Color:** `ChangeParticipantColor` RPC

**Example Group Message:**
```proto
Message {
  messageID: "group_msg_100"
  conversationID: "rcs-group-uuid-abc"
  participantID: "+1987654321"  // sender
  senderParticipant: {
    participantID: "+1987654321"
    name: "Alice"
    number: {number: "+1987654321"}
  }
  messageInfo: [{messageContent: {text: "Group message"}}]
}
```

---

## 5. Phone-side Dependencies

### 5.1 Required Android Configuration

**Google Messages App:**
- Must be installed and active
- Standard version from Google Play (no special build needed)
- No root access required
- Works with carrier RCS and Jibe-based RCS (Google's RCS server)

**Phone Requirements:**
- Must be online (connected to data or Wi-Fi) for desktop to function
- Bridge will not work if phone is turned off or data is disabled
- If phone goes offline, bridge sends `PhoneNotResponding` event after ~3 failed pings (8+ minutes)

**Permissions:**
- Google Messages app requests standard SMS/RCS permissions from Android
- No additional permissions needed for relay to function
- Bridge does NOT need phone's SMS database directly

**Network:**
- Phone connects to same Google relay servers as desktop
- Both communicate with `instantmessaging-pa.googleapis.com` or `instantmessaging-pa.clients6.google.com`
- No peer-to-peer communication; all via relay

### 5.2 Offline Behavior

**During Phone Offline Window:**
- Incoming messages queued on phone, delivered when phone reconnects
- Outgoing messages can still be sent (queued on server, phone will receive and send when online)
- Typing indicators and read receipts may be delayed

**Reconnection:**
- Phone re-joins long-poll, receives accumulated messages
- Bridge applies message deduplication (8-item hash ring buffer)
- No special reconnection RPC needed; phone auto-syncs

**Timeout Sequence:**
1. Ping sent every 1 minute
2. If ping timeout (default 1 min), first failure silent
3. After 3 failures (3+ minutes), emit `PhoneNotResponding` event
4. Keep pinging with exponential backoff (1m → 2m → 4m → 64m)
5. Upon response, emit `PhoneRespondingAgain`

### 5.3 Jibe RCS vs Carrier RCS

**No difference in protocol:**
- Both are treated identically at relay level
- Message type field set to `type: 4` (RCS) regardless of source
- Phone handles encryption/decryption transparently
- Bridge just passes plaintext to/from phone

---

## 6. Key Files in mautrix-gmessages

### Directory Structure

```
pkg/libgm/
├── client.go                 # Main Client struct, AuthData, encryption setup
├── pair.go                   # Phone relay pairing initiation
├── pair_google.go            # Gaia (Google account) pairing, UKEY2 flow (21KB)
├── session_handler.go        # RPC request/response lifecycle (8.6KB)
├── event_handler.go          # Incoming RPC decryption & routing (10KB)
├── longpoll.go               # Long-polling loop, ditto ping, connection mgmt (15KB)
├── http.go                   # HTTP client, protobuf marshaling (3.9KB)
├── media.go                  # Media upload/download, AES-GCM (11.7KB)
├── methods.go                # RPC method wrappers (SendMessage, etc.)
├── crypto/
│   ├── aesctr.go             # AES-CTR + HMAC encryption (1.8KB)
│   ├── aesgcm.go             # AES-GCM for media (3.4KB)
│   ├── ecdsa.go              # ECDSA key generation/handling (1.6KB)
│   └── generate.go           # Random key generation
├── gmproto/
│   ├── *.proto               # 9 proto files defining all message types
│   ├── *.pb.go               # Generated protobuf code (auto)
│   ├── rpc.proto             # Core RPC framing (177 lines)
│   ├── authentication.proto  # Auth + pairing (308 lines)
│   ├── conversations.proto   # Messages + chats
│   ├── events.proto          # Push event types
│   ├── config.proto          # Config version tracking
│   └── util.proto            # Common structures
├── util/
│   ├── constants.go          # API keys, user agents
│   ├── config.go             # Config version struct
│   ├── paths.go              # Endpoint URLs (33 lines)
│   └── http_helpers.go       # Header building
└── events/
    ├── ready.go
    ├── qr.go
    └── *.go                  # 10+ event type definitions
```

### 5 Most Important Files for Rust Port

**1. `pair_google.go` (21.4 KB)**
- **What:** UKEY2 pairing flow, emoji verification, shared secret derivation
- **Difficulty:** High (crypto + protocol state machine)
- **Rust Equivalents:** `elliptic_curve::ecdsa`, `sha2`, `hkdf`
- **Critical sections:**
  - `PairingSession.PreparePayloads()` (line 130-179): ECDSA key creation
  - `ProcessServerInit()` (line 218-284): ECDH + emoji extraction
  - `pairingEmojisV0/V1` (line 193-205): Hardcoded emoji list

**2. `longpoll.go` (15.2 KB)**
- **What:** Connection lifecycle, ditto ping logic, backoff algorithm
- **Difficulty:** Medium (async state machine)
- **Rust Equivalents:** `tokio::time::interval`, `futures::select!`
- **Critical sections:**
  - `dittoPinger.Loop()` (line 221-250): Main ping loop
  - `WaitForResponse()` (line 113-182): Exponential backoff implementation
  - Timeout constants (line 26-29): Tuning parameters

**3. `event_handler.go` (10.2 KB)**
- **What:** Incoming message decryption, route demultiplexing, type casting
- **Difficulty:** Low (straightforward unmarshaling)
- **Rust Equivalents:** `prost`, `base64`
- **Critical sections:**
  - `decryptInternalMessage()` (line 49-124): Main decrypt + route logic
  - `responseType` map (line 29-47): Action→Response type mapping

**4. `session_handler.go` (8.6 KB)**
- **What:** RPC request building, response routing, session tracking
- **Difficulty:** Low (straightforward request/response)
- **Rust Equivalents:** `uuid`, `prost`, channel handling
- **Critical sections:**
  - `buildMessage()` (line 189-250): Encryption + proto marshaling
  - `sendMessageWithParams()` (line 151-169): Request/response round-trip

**5. `http.go` (3.9 KB)**
- **What:** HTTP client setup, content-type handling, response parsing
- **Difficulty:** Low (standard HTTP + proto)
- **Rust Equivalents:** `reqwest` or `hyper`, `prost`, `mime`
- **Critical sections:**
  - `makeProtobufHTTPRequest()` (line 26-61): Transport setup
  - `decodeProtoResp()` (line 69-82): Content-type branching (PBLite vs protobuf)

---

## 7. Risks and Stability

### 7.1 Protocol Evolution

**Git History Analysis:**
- Project started August 2023 (v0.1.0)
- Active development: 2c2e5ad (latest as of May 2024)
- **30+ commits in 9 months** = relatively stable protocol
- Version bumping tied to calendar (v25.11, v26.01, v26.02, v26.04)

**Notable Protocol Changes:**
- **v0.4.3 (2024-07-16):** "Added support for new protocol version in Google account pairing"
  - Suggests Gaia auth flow changed; new `CreateGaiaPairingClientInit/Finish` actions added
- **v0.2.0 (2023-09-16):** Switched to "tablet mode" from "phone mode"
  - Device type changed; may affect how server routes messages
- **v0.6.0 (2024-12-16):** "Added support for re-authenticating expired Google logins"
  - Token refresh flow improved; less frequently needed to re-pair

**Config Version Bump:**
- `pkg/libgm/util/config.go` line 7-13: Currently set to Year=2026, Month=3, Day=18, V1=4, V2=6
- Implies Google server negotiates config version; may enforce minimum
- If server rejects config version, need to update constants

### 7.2 Known Issues & Workarounds

**From Code Comments:**

1. **Tablet Mode Limitation** (CHANGELOG.md v0.2.0):
   - Can't have 3+ devices online simultaneously
   - Bridge occupies one "tablet" slot
   - Two tablets + one web session max
   - **Workaround:** Existing users must re-pair after switching to tablet mode

2. **Dual-SIM Complexity** (methods.go line 121-129):
   - `TypingUpdateRequest` and `SendMessageRequest` include `SIMPayload`
   - Phone with dual SIM may send SIM ID for each message
   - Bridge must track which SIM is active per conversation

3. **Message Sync Race Condition** (CHANGELOG.md v0.4.0):
   - App sometimes stops sending new messages after long idle
   - **Workaround:** Ditto pinger + `HandleNoRecentUpdates()` trigger extra GET_UPDATES

4. **RCS Group Avatar Sync** (CHANGELOG.md v26.02):
   - Group avatars now supported but may race with group name changes
   - **Watch out:** Avatar field may arrive in separate update

5. **Uppercase Email Handling** (CHANGELOG.md v0.4.1):
   - Frequent disconnections if Google account has uppercase email
   - **Fix:** Lowercase device.SourceID during pairing (pair_google.go line 100)

6. **Incomplete Proto Definitions:**
   - Many fields named `unknownIntX`, `unknownBoolX`, etc. (e.g., authentication.proto line 148-161)
   - Fields are reverse-engineered; Google may add/remove without notice
   - Protobuf forward-compatibility helps (unknown fields ignored)

### 7.3 Google Account Blocking

**Risk Level:** Low (as of May 2024)

- **No reports** in CHANGELOG or code comments of mass bans
- Bridge uses standard Google APIs (SignInGaia, relay servers)
- Davirius credentials (cookies) are legitimate Google session tokens
- Device ID `messages-web-{uuid}` appears accepted by server

**Potential Risk Factors:**
1. If using same phone number on multiple desktops simultaneously (conflicts with tablet mode limit)
2. Abnormal message volume (bridge might be rate-limited per conversation)
3. Using very old config version (would need update)

**Recommended Test Setup:**
- Use **secondary Google account** for testing
- Keep primary account on main Google Messages app
- Have test phone as dedicated device (or second phone)
- Monitor for 403/401 errors in relay responses (would indicate auth revocation)

### 7.4 Rate Limiting

**Observable in Code:**
- No explicit backoff handling for 429 (Too Many Requests) responses
- HTTP 2-minute timeout may mask temporary rate-limit blocks
- Long-poll timeout 30 minutes (no keep-alive needed)

**Suspected Limits:**
- Per-user rate limit (from docs experience)
- Per-phone rate limit (to prevent message bombing)
- No per-message-type limits observed

**Best Practices:**
- Queue outgoing sends (don't flood with 100 messages/sec)
- Respect response timeouts; retry after exponential backoff
- Monitor 4xx/5xx error codes from HTTP layer

---

## 8. Porting Strategy to Rust

### 8.1 Go → Rust Library Mapping

| Functionality | Go Package | Rust Crate(s) | Notes |
|---------------|-----------|---------------|-------|
| **WebSocket** | stdlib `net/http` (long-poll) | `reqwest` + `tokio` | No WebSocket needed; use HTTP long-polling |
| **Protobuf** | `google.golang.org/protobuf` | `prost` | Code-gen from `.proto` files; must include custom PBLite |
| **Crypto: ECDSA** | `crypto/ecdsa` stdlib | `elliptic_curve::ecdsa` | P-256 curves, same as Go |
| **Crypto: SHA** | `crypto/sha256`, `crypto/sha512` | `sha2` | Standard hashing |
| **Crypto: HKDF** | `golang.org/x/crypto/hkdf` | `hkdf` | Key derivation |
| **Crypto: AES-CTR** | `crypto/aes` + `crypto/cipher` | `aes` + `ctr` from `cipher` | Mode of operation, not a separate crate |
| **Crypto: AES-GCM** | `crypto/aes` + `crypto/cipher` | `aes-gcm` | Authenticated encryption |
| **Crypto: HMAC** | `crypto/hmac` | `hmac` from `digest` crate | Message authentication |
| **Random Gen** | `crypto/rand` | `rand` | Random number generation |
| **HTTP Client** | `net/http` + `http.Client` | `reqwest::Client` | Async HTTP, TLS built-in |
| **JSON/Marshaling** | `encoding/json` | `serde_json` | Minimal use; mostly protobuf |
| **Async Runtime** | N/A (goroutines) | `tokio` | Multi-threaded async runtime |
| **Logging** | `github.com/rs/zerolog` | `tracing` + `tracing-subscriber` | Structured logging |
| **UUID** | `github.com/google/uuid` | `uuid` crate | ID generation |
| **Base64** | `encoding/base64` | `base64` | Encoding/decoding |

### 8.2 Hardest Pieces to Port

**1. PBLite Encoder/Decoder**
- **Issue:** mautrix-gmessages uses custom `go.mau.fi/util/pblite` package
- **Effort:** ~800 lines of Go code to port
- **Rust Option:** Either:
  - Port Go `pblite` code to Rust (medium effort, ~600 lines)
  - Fork `prost` with PBLite plugin (high effort, requires deep protobuf knowledge)
  - Contact mautrix maintainers to extract/share PBLite as standalone lib
- **Blocker:** Without PBLite, most RPC payloads won't serialize/deserialize

**2. UKEY2 Handshake State Machine**
- **Issue:** Requires precise sequencing of `ClientInit` → `ServerInit` → `ClientFinished` with ECDH + HKDF
- **Effort:** Medium (100-150 lines)
- **Rust Gotchas:**
  - ECDSA P-256 key generation and ECDH computation
  - Nonce/random generation must be cryptographically secure
  - SHA512 sum length handling (32 vs 64 bytes)

**3. Async Long-Poll Loop**
- **Issue:** Ditto ping backoff with exponential intervals, nested select! statements, connection retry logic
- **Effort:** High (requires deep async/await understanding)
- **Rust Gotchas:**
  - Cancellation safety (using `select!`)
  - Tokio task spawning for background pings
  - Channel handling for response routing
- **Go Advantage:** Goroutines + channels more elegant for this pattern
- **Rust Workaround:** Use structured concurrency (`tokio::spawn`, explicit shutdown signal)

**4. Request/Response Routing**
- **Issue:** Session handler maps RequestID → response channel, with 5-second timeout
- **Effort:** Low-medium (50 lines)
- **Rust Gotchas:**
  - Mutex contention if many concurrent requests
  - Timeout handling with `tokio::time::timeout`
  - Channel cleanup on timeout

### 8.3 Mechanical Ports (Lower Effort)

- **Encryption Functions:** AES-CTR, AES-GCM, HMAC-SHA256 → Direct mapping to `aes`, `aes-gcm`, `hmac` crates
- **Protocol Buffers:** Run `prost-build` on `.proto` files (copy from repo)
- **HTTP Transport:** `reqwest::Client` with custom headers (straightforward)
- **Config/Constants:** Copy string constants, adjust as needed
- **Event Definitions:** Mirror Go event types as Rust enums/structs

---

## 9. Caveats & Implementation Notes

### 9.1 PBLite Format Uncertainty

Several proto fields marked with `[(pblite.pblite_binary) = true]` indicate binary encoding within JSON-protobuf hybrid:
- `authentication.proto:58` — `SignInGaiaRequest_Inner_Data.someData` (line 58)
- `rpc.proto:88` — `OutgoingRPCMessage.destRegistrationIDs`
- `authentication.proto:148` — `RPCGaiaData.UnknownContainer.Item2.Item1.destOrSourceUUID`

**Impact:** These fields must be base64-encoded when PBLite is used. Failing to handle this will cause deserialization errors.

### 9.2 Config Version Staleness

Current config:
```go
var ConfigMessage = &gmproto.ConfigVersion{
  Year:  2026,
  Month: 3,
  Day:   18,
  V1:    4,
  V2:    6,
}
```

If Google updates their server to enforce a minimum config version and rejects old values, the Rust port must update these constants or implement dynamic version negotiation.

### 9.3 Device ID Format

For Gaia pairing, device ID is:
```
messages-web-{uuid without dashes}
```

Example: `messages-web-550e8400e29b41d4a716446655440000`

Must strip dashes from generated UUID before constructing device ID.

### 9.4 Message Type Discrimination

The `type` field in Message proto is non-standard:
- 1 = SMS
- 2 = MMS (downloaded/cached)
- 3 = MMS (not yet downloaded)
- 4 = RCS

This is **not** a protobuf enum; it's an int64. Future Google updates may add type 5, 6, etc. Code should handle unknown types gracefully.

### 9.5 Conversation ID Format

Conversation IDs differ by chat type:
- **1-on-1 SMS/MMS:** Phone number (e.g., "+12125551234")
- **1-on-1 RCS:** May be same as phone number or a UUID
- **Group RCS:** UUID (e.g., "f47ac10b-58cc-4372-a567-0e02b2c3d479")
- **Group MMS:** May be abbreviated hash

Code should not assume phone number format; treat all as opaque strings.

---

## 10. Testing Recommendations

### 10.1 Secondary Account Setup

1. Create a Google account separate from your main email
2. Set up a secondary Android phone (or emulator) with Google Messages installed
3. Pair the Rust implementation with this secondary account
4. Use for:
   - Protocol development testing
   - Integration tests (actual message send/receive)
   - Regression testing on new releases

### 10.2 Minimal Reproducible Example

For quick testing without a full bridge:
1. Implement only pairing + `ListConversations` RPC
2. Verify QR code generation and emoji verification flow
3. Confirm token refresh works
4. Then add message send/receive

### 10.3 Monitoring Points

- HTTP status codes: Watch for 403 (auth fail), 429 (rate limit), 500+ (server error)
- Protobuf parse errors: Log unknown fields, parse failures
- Async timeout: Track how often 5-second timeout is hit; may indicate server lag
- Ditto ping: Monitor ping RTT, backoff iteration count

---

## Conclusion

The Google Messages web protocol is a mature, stable REST+Protobuf relay architecture. The hardest parts to port are:
1. **PBLite encoder/decoder** (requires understanding custom encoding)
2. **UKEY2 pairing** (requires ECDSA + HKDF)
3. **Long-poll + ditto ping async loop** (Rust async patterns)

Everything else is mechanical: standard crypto primitives, protobuf code-gen, HTTP requests. The protocol has been stable for ~9 months with only minor version bumps, suggesting the wire format is unlikely to break in the near term.

**Estimated effort:** 4-6 weeks for a working Rust implementation, assuming:
- 2 weeks: PBLite + protobuf setup
- 1 week: Pairing (UKEY2 + Gaia)
- 1 week: HTTP transport + long-poll
- 1-2 weeks: RPC methods + event handling
- 1 week: Testing & debugging

Good luck with the port!
