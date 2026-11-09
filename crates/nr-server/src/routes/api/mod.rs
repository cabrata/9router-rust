pub mod common;
pub mod combos;
pub mod health;
pub mod init;
pub mod keys;
pub mod locale;
pub mod media_providers;
pub mod models;
pub mod pricing;
pub mod provider_nodes;
pub mod providers;
pub mod proxy_pools;
pub mod settings;
pub mod shutdown;
pub mod tags;
pub mod version;

use axum::routing::{get, post, put};
use axum::Router;

/// Export all /api management REST routes
pub fn api_routes() -> Router {
    Router::new()
        // Health
        .route("/api/health", get(health::get_health).options(health::options_health))
        // Init
        .route("/api/init", get(init::get_init))
        // Locale
        .route("/api/locale", post(locale::post_locale))
        // Shutdown
        .route("/api/shutdown", post(shutdown::post_shutdown))
        // Tags
        .route("/api/tags", get(tags::get_tags).options(tags::options_tags))
        // Version
        .route("/api/version", get(version::get_version))
        .route("/api/version/shutdown", post(version::post_version_shutdown))
        .route("/api/version/update", post(version::post_version_update))
        // Pricing
        .route(
            "/api/pricing",
            get(pricing::get_pricing_handler)
                .patch(pricing::patch_pricing)
                .delete(pricing::delete_pricing),
        )
        .route("/api/pricing/defaults", get(pricing::get_pricing_defaults))
        // Keys
        .route("/api/keys", get(keys::get_keys).post(keys::post_keys))
        .route(
            "/api/keys/{id}",
            get(keys::get_key_by_id_handler)
                .put(keys::put_key_by_id)
                .delete(keys::delete_key_by_id),
        )
        // Combos
        .route(
            "/api/combos",
            get(combos::get_combos_handler).post(combos::post_combos),
        )
        .route(
            "/api/combos/presets",
            get(combos::get_combos_presets).post(combos::post_combos_presets),
        )
        .route(
            "/api/combos/{id}",
            get(combos::get_combo_by_id_handler)
                .put(combos::put_combo_by_id)
                .delete(combos::delete_combo_by_id),
        )
        // Settings
        .route(
            "/api/settings",
            get(settings::get_settings_handler).patch(settings::patch_settings),
        )
        .route(
            "/api/settings/database",
            get(settings::get_settings_database).post(settings::post_settings_database),
        )
        .route(
            "/api/settings/proxy-test",
            post(settings::post_settings_proxy_test),
        )
        .route(
            "/api/settings/require-login",
            get(settings::get_settings_require_login),
        )
        // Models
        .route(
            "/api/models",
            get(models::get_models_handler).put(models::put_models),
        )
        .route(
            "/api/models/alias",
            get(models::get_models_alias)
                .put(models::put_models_alias)
                .delete(models::delete_models_alias),
        )
        .route(
            "/api/models/availability",
            get(models::get_models_availability).post(models::post_models_availability),
        )
        .route(
            "/api/models/catalog-sync",
            get(models::get_models_catalog_sync).post(models::post_models_catalog_sync),
        )
        .route(
            "/api/models/custom",
            get(models::get_models_custom)
                .post(models::post_models_custom)
                .delete(models::delete_models_custom),
        )
        .route(
            "/api/models/disabled",
            get(models::get_models_disabled)
                .post(models::post_models_disabled)
                .delete(models::delete_models_disabled),
        )
        .route("/api/models/test", post(models::post_models_test))
        // Provider Nodes
        .route(
            "/api/provider-nodes",
            get(provider_nodes::get_provider_nodes_handler).post(provider_nodes::post_provider_nodes),
        )
        .route(
            "/api/provider-nodes/validate",
            post(provider_nodes::post_provider_nodes_validate),
        )
        .route(
            "/api/provider-nodes/{id}",
            put(provider_nodes::put_provider_node_by_id)
                .delete(provider_nodes::delete_provider_node_by_id),
        )
        // Proxy Pools
        .route(
            "/api/proxy-pools",
            get(proxy_pools::get_proxy_pools_handler).post(proxy_pools::post_proxy_pools),
        )
        .route(
            "/api/proxy-pools/cloudflare-deploy",
            post(proxy_pools::post_proxy_pools_cloudflare_deploy),
        )
        .route(
            "/api/proxy-pools/deno-deploy",
            post(proxy_pools::post_proxy_pools_deno_deploy),
        )
        .route(
            "/api/proxy-pools/netlify-deploy",
            post(proxy_pools::post_proxy_pools_netlify_deploy),
        )
        .route(
            "/api/proxy-pools/vercel-deploy",
            post(proxy_pools::post_proxy_pools_vercel_deploy),
        )
        .route(
            "/api/proxy-pools/{id}",
            get(proxy_pools::get_proxy_pool_by_id_handler)
                .put(proxy_pools::put_proxy_pool_by_id)
                .delete(proxy_pools::delete_proxy_pool_by_id),
        )
        .route(
            "/api/proxy-pools/{id}/test",
            post(proxy_pools::post_proxy_pool_test),
        )
        // Media Providers (TTS)
        .route(
            "/api/media-providers/tts/voices",
            get(media_providers::get_tts_voices),
        )
        .route(
            "/api/media-providers/tts/elevenlabs/voices",
            get(media_providers::get_elevenlabs_voices),
        )
        .route(
            "/api/media-providers/tts/deepgram/voices",
            get(media_providers::get_deepgram_voices),
        )
        .route(
            "/api/media-providers/tts/inworld/voices",
            get(media_providers::get_inworld_voices),
        )
        .route(
            "/api/media-providers/tts/minimax/voices",
            get(media_providers::get_minimax_voices),
        )
        // Providers
        .route(
            "/api/providers",
            get(providers::get_providers_handler).post(providers::post_providers),
        )
        .route(
            "/api/providers/client",
            get(providers::get_providers_client),
        )
        .route(
            "/api/providers/kilo/free-models",
            get(providers::get_kilo_free_models),
        )
        .route(
            "/api/providers/suggested-models",
            get(providers::get_suggested_models),
        )
        .route(
            "/api/providers/test-batch",
            post(providers::post_providers_test_batch),
        )
        .route(
            "/api/providers/validate",
            post(providers::post_providers_validate),
        )
        .route(
            "/api/providers/web-login",
            get(providers::get_providers_web_login)
                .post(providers::post_providers_web_login)
                .delete(providers::delete_providers_web_login),
        )
        .route(
            "/api/providers/{id}",
            get(providers::get_provider_by_id_handler)
                .put(providers::put_provider_by_id)
                .delete(providers::delete_provider_by_id),
        )
        .route(
            "/api/providers/{id}/models",
            get(providers::get_provider_models_handler),
        )
        .route(
            "/api/providers/{id}/overrides",
            get(providers::get_provider_overrides).put(providers::put_provider_overrides),
        )
        .route(
            "/api/providers/{id}/test",
            post(providers::post_provider_test),
        )
        .route(
            "/api/providers/{id}/test-models",
            post(providers::post_provider_test_models),
        )
}
