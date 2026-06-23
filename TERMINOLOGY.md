# Telegraph Center Terminology

Shared language for the service that receives audio captures, transcribes them, and delivers them to configured destinations.

## Language

**Recording**:
One completed audio capture submitted by a client, optionally with client-provided tags, and tracked through transcription and delivery.
_Avoid_: task, message, submission.

**Client**:
An authenticated producer that submits Recordings to Telegraph Center.
_Avoid_: user, device, account.

**Client Certificate Fingerprint**:
The stable certificate-derived identity value that Telegraph Center uses to map an mTLS-authenticated request to a configured Client.
_Avoid_: username, device ID, common name.

**Client Recording ID**:
The Client-assigned identifier for a Recording, stable across upload retries and unique within that Client's submitted Recordings.
_Avoid_: filename, upload ID, request ID.

**Backlog**:
The holding area for Recordings that have not been delivered because automatic routing did not select a Sink.
_Avoid_: dead letter queue, failures, archive.

**Transcript**:
The text derived from a Recording by speech-to-text processing.
_Avoid_: transcription when referring to the resulting text.

**Transcription**:
The process of deriving a Transcript from a Recording.
_Avoid_: transcript when referring to the processing work.

**Sink**:
A configured destination that receives a Transcript payload and related Recording metadata.
_Avoid_: audio destination, storage backend, ad hoc webhook.

**Webhook Sink**:
A Sink that delivers a Transcript payload by making an HTTP webhook request.
_Avoid_: Hermes Sink, callback, HTTP task.

**Routing Rule**:
A configured rule that selects a Sink for a Recording.
_Avoid_: hard-coded route, classifier, destination.

**Manual Routing**:
An operator action that selects a Sink for a Backlogged Recording.
_Avoid_: reprocessing, retry, editing.

**Delivery**:
The logical act of sending a Transcript payload for a Recording to a selected Sink.
_Avoid_: routing, upload, dispatch.

**Delivery Attempt**:
One try at completing a Delivery.
_Avoid_: delivery when referring to a single retryable try.

**Operator**:
A human who can access the monitoring webapp and perform operational recovery actions such as Manual Routing or retrying failed work.
_Avoid_: user, admin, account.

**Operator Session**:
An authenticated browser session for an Operator using the monitoring webapp.
_Avoid_: login token, API key, client session.

## Example Dialogue

Developer: "What does litewatch upload after the user stops recording?"

Domain expert: "It uploads a Recording. Telegraph Center then transcribes and routes that Recording."

Developer: "If litewatch retries the same upload, is that a new Recording?"

Domain expert: "No. The same Client Recording ID from the same Client refers to the same Recording."

Developer: "How does Telegraph Center know which Client submitted a Recording?"

Domain expert: "Nginx verifies mTLS and passes a Client Certificate Fingerprint that Telegraph Center maps to a configured Client."

Developer: "Is a Backlogged Recording stuck forever?"

Domain expert: "No. An operator can use Manual Routing from the monitoring web page to send it to a Sink."

Developer: "Does a Sink receive the original audio?"

Domain expert: "No. The Sink receives the Transcript payload and related Recording metadata; Telegraph Center keeps the audio."

Developer: "Is Transcription part of the upload request?"

Domain expert: "No. The upload completes after the Recording is durably stored; Transcription runs afterward."

Developer: "What happens when no Routing Rule selects a Sink?"

Domain expert: "The Recording enters the Backlog, where an operator can use Manual Routing."

Developer: "If a Webhook Sink is down, does the Recording go back to the Backlog?"

Domain expert: "No. The Recording has a selected Sink; its Delivery is failing or retrying."

Developer: "Can the monitor be viewed from a phone outside the homelab?"

Domain expert: "Yes, through an Operator Session protected by the app's username and password login."
