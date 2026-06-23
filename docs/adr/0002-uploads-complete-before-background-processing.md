# Uploads Complete Before Background Processing

Telegraph Center accepts a Recording upload only through durable local storage, then performs transcription, routing, and delivery in background workers. This keeps client uploads short and reliable, lets litewatch retry safely, and allows Soniox or Sink outages to be handled without rejecting newly uploaded audio.
