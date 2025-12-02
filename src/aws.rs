use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use aws_sdk_s3::types::{MetadataDirective, RestoreRequest};
use chrono::{DateTime, Utc};

use crate::models::{BucketInfo, ObjectInfo, RestoreState, StorageClassTier};

pub struct S3Service {
    client: Client,
}

impl S3Service {
    pub async fn new() -> Result<Self> {
        let config = aws_config::from_env().load().await;
        let client = Client::new(&config);
        Ok(Self { client })
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
        Ok(constraint.and_then(|c| {
            if c.as_str().is_empty() {
                None
            } else {
                Some(c.as_str().to_string())
            }
        }))
    }

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<ObjectInfo>> {
        let mut continuation_token: Option<String> = None;
        let mut objects = Vec::new();
        loop {
            let mut request = self.client.list_objects_v2().bucket(bucket);
            if let Some(token) = &continuation_token {
                request = request.continuation_token(token);
            }
            if let Some(pref) = prefix {
                request = request.prefix(pref);
            }
            let response = request.send().await?;
            for object in response.contents() {
                if let Some(key) = object.key() {
                    objects.push(ObjectInfo {
                        key: key.to_string(),
                        size: object.size().unwrap_or_default(),
                        last_modified: object.last_modified().map(|dt| dt.to_string()),
                        storage_class: StorageClassTier::from(object.storage_class().cloned()),
                        restore_state: None,
                    });
                }
            }

            if response.is_truncated().unwrap_or(false) {
                continuation_token = response
                    .next_continuation_token()
                    .map(|token| token.to_string());
            } else {
                break;
            }
        }
        Ok(objects)
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
