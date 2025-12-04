use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use aws_sdk_s3::types::{MetadataDirective, RestoreRequest};
use chrono::{DateTime, Utc};

use crate::models::{BucketInfo, ObjectInfo, RestoreState, StorageClassTier};

pub struct S3Service {
    client: Client,
    region: Option<String>,
}

impl S3Service {
    pub async fn new() -> Result<Self> {
        let config = aws_config::from_env().load().await;
        let region = config.region().map(|r| r.as_ref().to_string());
        let client = Client::new(&config);
        Ok(Self { client, region })
    }

    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub async fn list_buckets(&self) -> Result<Vec<BucketInfo>> {
        let output = self.client.list_buckets().send().await?;
        let mut buckets = Vec::new();
        for bucket in output.buckets() {
            if let Some(name) = bucket.name() {
                let region = self.get_bucket_region(name).await.unwrap_or(None);
                let created = bucket.creation_date().map(|dt| dt.to_string());
                buckets.push(BucketInfo {
                    name: name.to_string(),
                    region,
                    creation_date: created,
                });
            }
        }
        buckets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(buckets)
    }

    async fn get_bucket_region(&self, bucket: &str) -> Result<Option<String>> {
        let resp = self
            .client
            .get_bucket_location()
            .bucket(bucket)
            .send()
            .await?;
        let constraint = resp.location_constraint();
        Ok(constraint
            .map(|c| {
                let region_str = c.as_str();
                if region_str.is_empty() {
                    "us-east-1".to_string()
                } else {
                    region_str.to_string()
                }
            })
            .or(Some("us-east-1".to_string())))
    }

    /// Count total objects in bucket without loading full details (fast)
    pub async fn count_objects(&self, bucket: &str, prefix: Option<&str>) -> Result<usize> {
        let mut continuation_token: Option<String> = None;
        let mut total_count = 0;
        loop {
            let mut request = self.client.list_objects_v2().bucket(bucket);
            if let Some(token) = &continuation_token {
                request = request.continuation_token(token);
            }
            if let Some(pref) = prefix {
                request = request.prefix(pref);
            }
            let response = request.send().await?;
            total_count += response.key_count().unwrap_or(0) as usize;

            if response.is_truncated().unwrap_or(false) {
                continuation_token = response
                    .next_continuation_token()
                    .map(|token| token.to_string());
            } else {
                break;
            }
        }
        Ok(total_count)
    }

    /// Load a page of objects with optional continuation token
    pub async fn list_objects_paginated(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        continuation_token: Option<String>,
        max_keys: i32,
    ) -> Result<(Vec<ObjectInfo>, Option<String>)> {
        let mut request = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .max_keys(max_keys);
        if let Some(token) = continuation_token {
            request = request.continuation_token(token);
        }
        if let Some(pref) = prefix {
            request = request.prefix(pref);
        }
        let response = request.send().await?;

        let mut objects = Vec::new();
        for object in response.contents() {
            if let Some(key) = object.key() {
                // Note: ListObjectsV2 does not return restore status, it's always None
                // We fetch it separately for Glacier objects after loading
                objects.push(ObjectInfo {
                    key: key.to_string(),
                    size: object.size().unwrap_or_default(),
                    last_modified: object.last_modified().map(|dt| dt.to_string()),
                    storage_class: StorageClassTier::from(object.storage_class().cloned()),
                    restore_state: None, // Will be populated by batch_refresh_restore_status
                });
            }
        }

        let next_token = if response.is_truncated().unwrap_or(false) {
            response.next_continuation_token().map(|t| t.to_string())
        } else {
            None
        };

        Ok((objects, next_token))
    }

    pub async fn refresh_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo> {
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await?;

        Ok(ObjectInfo {
            key: key.to_string(),
            size: head.content_length().unwrap_or_default(),
            last_modified: head.last_modified().map(|dt| dt.to_string()),
            storage_class: StorageClassTier::from(head.storage_class().cloned()),
            restore_state: parse_restore_state(head.restore()),
        })
    }

    /// Batch refresh restore status for Glacier objects
    /// Returns a map of key -> restore_state
    pub async fn batch_refresh_restore_status(
        &self,
        bucket: &str,
        keys: &[String],
    ) -> Vec<(String, Option<RestoreState>)> {
        let mut results = Vec::new();

        // Make concurrent HeadObject calls (but limit concurrency)
        use futures::stream::{self, StreamExt};

        let chunk_size = 10; // Process 10 at a time
        let mut stream = stream::iter(keys)
            .map(|key| {
                let bucket = bucket.to_string();
                let key = key.to_string();
                async move {
                    match self
                        .client
                        .head_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                    {
                        Ok(head) => {
                            let restore_state = parse_restore_state(head.restore());
                            (key, restore_state)
                        }
                        Err(_) => {
                            // If HeadObject fails, keep the status unknown
                            (key, None)
                        }
                    }
                }
            })
            .buffer_unordered(chunk_size);

        while let Some(result) = stream.next().await {
            results.push(result);
        }

        results
    }

    pub async fn transition_storage_class(
        &self,
        bucket: &str,
        key: &str,
        target: StorageClassTier,
    ) -> Result<()> {
        let storage_class = target
            .to_sdk()
            .context("target storage class is not supported via API")?;
        let source = format!("{}/{}", bucket, key);
        let encoded_source = urlencoding::encode(&source).into_owned();
        self.client
            .copy_object()
            .bucket(bucket)
            .key(key)
            .storage_class(storage_class)
            .copy_source(encoded_source)
            .metadata_directive(MetadataDirective::Copy)
            .send()
            .await?;
        Ok(())
    }

    pub async fn request_restore(&self, bucket: &str, key: &str, days: i32) -> Result<()> {
        let restore_request = RestoreRequest::builder().days(days).build();

        self.client
            .restore_object()
            .bucket(bucket)
            .key(key)
            .restore_request(restore_request)
            .send()
            .await?;

        Ok(())
    }
}

fn parse_restore_state(raw: Option<&str>) -> Option<RestoreState> {
    raw.map(|value| {
        let value = value.to_ascii_lowercase();
        if value.contains("ongoing-request=\"true\"") {
            RestoreState::InProgress { expiry: None }
        } else if let Some(expiry) = value
            .split("expiry-date=\"")
            .nth(1)
            .and_then(|part| part.split('"').next())
        {
            DateTime::parse_from_rfc2822(expiry)
                .map(|dt| RestoreState::InProgress {
                    expiry: Some(dt.with_timezone(&Utc).to_rfc3339()),
                })
                .unwrap_or(RestoreState::Available)
        } else if value.contains("ongoing-request=\"false\"") {
            RestoreState::Available
        } else {
            RestoreState::Expired
        }
    })
}
