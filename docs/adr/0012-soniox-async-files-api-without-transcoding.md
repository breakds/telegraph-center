# Soniox Async Files API Without Transcoding

Telegraph Center submits stored audio to Soniox using the Speech-to-Text async flow: upload the local audio file to the Soniox Files API, create a transcription using the returned file ID, poll for completion, retrieve the transcript, and clean up Soniox-side file/transcription resources. Soniox supports async transcription for uploaded local files and auto-detects supported audio formats including WAV, so v1 does not transcode or resample audio before submission.

Telegraph Center's 256 MiB upload limit is an intake and storage limit, not a guarantee that Soniox will accept every stored Recording. If Soniox rejects a file due to provider size or quota limits, the Recording remains stored locally and Transcription is marked failed with the provider error visible to the Operator.
