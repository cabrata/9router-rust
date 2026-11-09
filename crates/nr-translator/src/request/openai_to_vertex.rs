//! OpenAI to Vertex request translator. Port of open-sse/translator/request/openai-to-vertex.js.

use nr_core::{Credentials, Json};
use crate::{TranslationOptions};
use super::openai_to_gemini::openai_to_gemini_request;

const DEFAULT_THINKING_VERTEX_SIGNATURE: &str = "CloBjz1rX5+yg1ILh/Ag+suum5k1f/9m/hI0XDQ33lsQIYnOHLn9KZwN0C7E4jgep5MzZvz5Se1Z1xxYrA1+Iz0Il4tabBhaDfMKNa5dGdEA3KnikfjfIpMlPaAKaQGPPWtf2hodPdBgguiZqDn+Qz2LwGEqHVJ16LVBUpeSx7UnYBLSwio8cyNy0jPijOh5QXKLTeHVdO2tKKcCCrtG2JCW3dOSrW2qA8eyAg40iQUnMNECjbcjkqB1+zrab7jX9ILwg7L9OgqYAQGPPWtfT4nzaPzSkXePAa920abYxPs3fg/RHDlg8PUVFLa+ko6qOjt7nXJTMxN0cpCwUCFX7eHHcMnA6vApyA/rXvJiAABkHZ3HilAktXRtxr/thHU0H8/4H5gT3kzoQcq9aMznrKomd3ct0mFi0ioSKnOEfoY1Mrfj00p/ZWm0tT7Wrjcm3BQXZ+T9Vrb94k+6CjtcEBrGCq8BAY89a1/vTMczqwB1NP3HCCuBdnds2vDXkj6XAYaXjsjmik8tGqwMKHz8R9RAWsx6SO6pkGEpXXpRzAaUx6c+aofsL/z1xOcN7ArCAa6uEeQKEgNngZuCP05p4+9P95epVmgOjFa4KfsPnyg+NKUkEFmpPSDrIRyMT+xERlclVcCI98/u7i8a9+vTbgzl8TRFYryClNH37K1ye5i6kqSGDUcMyiEasjke5BxbUh3i6wqMAgGPPWtfk+A+iY38QAldu117FEkTIkzbYOIt67lk9c6Ou3Y3Ct8TFHFw5QwGfSFc0YWjeTFHdm9UdV5jPK35p6VfhiRSva3w2+JLIHb4jvv5HutZPOJ3yQTt/+hUDj80oMNMbwnxNZvCEdzKS+D9vwmTACAm5H0ZetBSH2gPJXnhhuQo9AegS3wIWVR2a5k643Vx9r4u4pOvij4476lxKswIHvqsjL4jnTzRCvd44G6dn7vD0ENGb1K/i+dMRQMcOBaOxPN0ynk9bKxXWRDbZ+Rhakfr+y74z+6eYCdRPVqO9I7s+riilFuRIfaQ+U6/vuVKGIWEVKCfZZi0z6H5Xgz1xmse0u0AsittDlIKxwEBjz1rX783A0vehvUeabRia+/pX46IsN5efTAxFEBUeUce3jLuXIghkMV2b8KNhUs2G0aZldDDewRQbkluQabBMDT82N5I7reJP0VZgLIKccCL5DoGv1J7YWM2npLMIgZ6aP8aSlT3PFFJ0IXbUZUrzduczmIm6nzAJf9zxmq1aIFYw8YrgW8RjUdy0UvUmRoBEShSGUrvsyaRTl7J//KJW5utIPunFMu53GPWLidCFHzM1QA3Cj1+4zv5UXajP/V92RQayWbzCvYBAY89a1+yzoVSWukUGH7kX71Tg9dx7HA7OyKYwnYaqekG98zJfcUM/3KoiiiotW5t4xYu//ksEl36bSWvUHsRnxGByg+3WYdnZqKg0AtdRB/EXbI5PsjvS5ko96bkjSuFkY3TjHGwAM2B94K6/t6OTE/NBbxCsY9sT4d+1sbFv/iyfmfCnfvJaSzGmC9CDWKy4iqQ/vBNWps9j1JXk0p5uPAYC2BaMkxl5xoTVZqI3zAuRtQF5JLmPPy+PdqOgFxMKcLGNhwp7dbhIFLF68vCYQ9CL0NnK2d3CFk1UFVYxsi1TsolR1xahe/Rxt5HZDz/z65nevrQ";

pub fn openai_to_vertex_request(
    model: &str,
    body: Json,
    stream: bool,
    credentials: Option<&mut Credentials>,
    options: &TranslationOptions,
) -> Json {
    // Convert to Gemini first
    let mut gemini_body = openai_to_gemini_request(model, body, stream, credentials, options);

    // Post-process for Vertex
    if let Some(contents) = gemini_body.get_mut("contents").and_then(|v| v.as_array_mut()) {
        for turn in contents.iter_mut() {
            if let Some(parts) = turn.get_mut("parts").and_then(|v| v.as_array_mut()) {
                for part in parts.iter_mut() {
                    if let Some(obj) = part.as_object_mut() {
                        // Replace thoughtSignature with Vertex-native one
                        if obj.contains_key("thoughtSignature") {
                            obj.insert("thoughtSignature".to_string(), Json::String(DEFAULT_THINKING_VERTEX_SIGNATURE.to_string()));
                        }
                        // Strip id from functionCall
                        if let Some(fc) = obj.get_mut("functionCall").and_then(|v| v.as_object_mut()) {
                            fc.remove("id");
                        }
                        // Strip id from functionResponse
                        if let Some(fr) = obj.get_mut("functionResponse").and_then(|v| v.as_object_mut()) {
                            fr.remove("id");
                        }
                    }
                }
            }
        }
    }

    gemini_body
}
