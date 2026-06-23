# At-Least-Once Webhook Delivery

Webhook Sink delivery is at least once: Telegraph Center retries failed HTTP deliveries and reuses a stable Delivery ID across attempts. Exactly-once delivery is not attempted over HTTP; downstream systems such as Hermes can deduplicate using the Delivery ID carried in the payload and request headers.
