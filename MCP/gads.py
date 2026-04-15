from __future__ import annotations

import os
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from typing import Any

from dotenv import load_dotenv
from google.ads.googleads.client import GoogleAdsClient
from google.ads.googleads.errors import GoogleAdsException
from google.api_core import protobuf_helpers
from mcp.server.fastmcp import FastMCP


ROOT_DIR = Path(__file__).resolve().parent
ENV_PATH = ROOT_DIR / ".env"

mcp = FastMCP("mcp-gads-striker")


class ConfigError(RuntimeError):
    """Raised when required Google Ads configuration is missing."""


class GoogleAdsToolError(RuntimeError):
    """Raised when a Google Ads operation fails."""


@dataclass(frozen=True)
class Settings:
    developer_token: str
    client_id: str
    client_secret: str
    refresh_token: str
    login_customer_id: str
    customer_id: str


def _digits_only(value: str) -> str:
    return "".join(ch for ch in value if ch.isdigit())


def _load_env_file() -> None:
    if ENV_PATH.exists():
        load_dotenv(ENV_PATH, override=False)


@lru_cache(maxsize=1)
def get_settings() -> Settings:
    _load_env_file()
    required_keys = (
        "DEVELOPER_TOKEN",
        "CLIENT_ID",
        "CLIENT_SECRET",
        "REFRESH_TOKEN",
        "LOGIN_CUSTOMER_ID",
    )
    values: dict[str, str] = {}
    missing: list[str] = []
    for key in required_keys:
        value = os.getenv(key, "").strip()
        if not value:
            missing.append(key)
        else:
            values[key] = value
    if missing:
        joined = ", ".join(missing)
        raise ConfigError(f"Missing required Google Ads settings in .env: {joined}")

    login_customer_id = _digits_only(values["LOGIN_CUSTOMER_ID"])
    if not login_customer_id:
        raise ConfigError("LOGIN_CUSTOMER_ID must contain digits.")

    configured_customer_id = _digits_only(os.getenv("CUSTOMER_ID", "").strip())
    customer_id = configured_customer_id or login_customer_id

    return Settings(
        developer_token=values["DEVELOPER_TOKEN"],
        client_id=values["CLIENT_ID"],
        client_secret=values["CLIENT_SECRET"],
        refresh_token=values["REFRESH_TOKEN"],
        login_customer_id=login_customer_id,
        customer_id=customer_id,
    )


@lru_cache(maxsize=1)
def get_google_ads_client() -> GoogleAdsClient:
    settings = get_settings()
    configuration = {
        "developer_token": settings.developer_token,
        "client_id": settings.client_id,
        "client_secret": settings.client_secret,
        "refresh_token": settings.refresh_token,
        "login_customer_id": settings.login_customer_id,
        "use_proto_plus": True,
    }
    return GoogleAdsClient.load_from_dict(configuration)


def _extract_resource_id(resource_name: str) -> str:
    if "~" in resource_name:
        return resource_name.rsplit("~", maxsplit=1)[-1]
    return resource_name.rsplit("/", maxsplit=1)[-1]


def _coerce_date_filter(date_range: str) -> str:
    normalized = date_range.strip().upper()
    predefined = {
        "TODAY",
        "YESTERDAY",
        "LAST_7_DAYS",
        "LAST_14_DAYS",
        "LAST_30_DAYS",
        "LAST_BUSINESS_WEEK",
        "LAST_WEEK_MON_SUN",
        "LAST_WEEK_SUN_SAT",
        "THIS_MONTH",
        "LAST_MONTH",
        "ALL_TIME",
    }
    if normalized in predefined:
        return f"segments.date DURING {normalized}"

    if ":" in date_range:
        start_date, end_date = [part.strip() for part in date_range.split(":", maxsplit=1)]
        if start_date and end_date:
            return f"segments.date BETWEEN '{start_date}' AND '{end_date}'"

    raise ValueError(
        "date_range must be a Google Ads DURING token like LAST_7_DAYS "
        "or a custom range in YYYY-MM-DD:YYYY-MM-DD format."
    )


def _validate_match_type(match_type: str) -> str:
    normalized = match_type.strip().upper()
    if normalized not in {"EXACT", "PHRASE"}:
        raise ValueError("match_type must be EXACT or PHRASE.")
    return normalized


def _validate_entity_type(entity_type: str) -> str:
    normalized = entity_type.strip().upper()
    if normalized not in {"CAMPAIGN", "AD_GROUP"}:
        raise ValueError("entity_type must be CAMPAIGN or AD_GROUP.")
    return normalized


def _validate_assets(
    headlines: list[str],
    descriptions: list[str],
) -> tuple[list[str], list[str]]:
    cleaned_headlines = [headline.strip() for headline in headlines if headline.strip()]
    cleaned_descriptions = [
        description.strip() for description in descriptions if description.strip()
    ]

    if len(cleaned_headlines) < 3:
        raise ValueError("Responsive Search Ads require at least 3 non-empty headlines.")
    if len(cleaned_descriptions) < 2:
        raise ValueError("Responsive Search Ads require at least 2 non-empty descriptions.")
    if len(cleaned_headlines) > 15:
        raise ValueError("Responsive Search Ads support at most 15 headlines.")
    if len(cleaned_descriptions) > 4:
        raise ValueError("Responsive Search Ads support at most 4 descriptions.")

    too_long_headlines = [headline for headline in cleaned_headlines if len(headline) > 30]
    too_long_descriptions = [
        description for description in cleaned_descriptions if len(description) > 90
    ]
    if too_long_headlines:
        raise ValueError(
            "Each headline must be 30 characters or fewer. "
            f"Invalid headlines: {too_long_headlines}"
        )
    if too_long_descriptions:
        raise ValueError(
            "Each description must be 90 characters or fewer. "
            f"Invalid descriptions: {too_long_descriptions}"
        )

    return cleaned_headlines, cleaned_descriptions


def _search(query: str, customer_id: str | None = None) -> Any:
    client = get_google_ads_client()
    google_ads_service = client.get_service("GoogleAdsService")
    return google_ads_service.search(
        customer_id=customer_id or get_settings().customer_id,
        query=query,
    )


def _get_campaign_resource_name(campaign_id: str) -> str:
    client = get_google_ads_client()
    return client.get_service("CampaignService").campaign_path(
        get_settings().customer_id,
        _digits_only(campaign_id),
    )


def _get_ad_group_resource_name(ad_group_id: str) -> str:
    client = get_google_ads_client()
    return client.get_service("AdGroupService").ad_group_path(
        get_settings().customer_id,
        _digits_only(ad_group_id),
    )


def _lookup_campaign_budget_resource_name(campaign_id: str) -> str:
    cleaned_campaign_id = _digits_only(campaign_id)
    query = f"""
        SELECT
          campaign.campaign_budget
        FROM campaign
        WHERE campaign.id = {cleaned_campaign_id}
        LIMIT 1
    """
    rows = list(_search(query))
    if not rows:
        raise GoogleAdsToolError(f"Campaign {cleaned_campaign_id} was not found.")
    return rows[0].campaign.campaign_budget


def _build_error_payload(exc: Exception) -> dict[str, Any]:
    if isinstance(exc, GoogleAdsException):
        return {
            "error": exc.error.code().name,
            "message": exc.failure.errors[0].message if exc.failure.errors else str(exc),
            "request_id": exc.request_id,
        }
    return {"error": exc.__class__.__name__, "message": str(exc)}


@mcp.tool()
def gads_get_metrics(campaign_id: str, date_range: str) -> dict[str, Any]:
    """Fetch campaign telemetry for a date range using GAQL."""
    cleaned_campaign_id = _digits_only(campaign_id)
    if not cleaned_campaign_id:
        raise ValueError("campaign_id must contain digits.")

    date_filter = _coerce_date_filter(date_range)
    query = f"""
        SELECT
          campaign.id,
          campaign.name,
          metrics.impressions,
          metrics.clicks,
          metrics.cost_micros,
          metrics.average_cpc,
          metrics.ctr
        FROM campaign
        WHERE campaign.id = {cleaned_campaign_id}
          AND {date_filter}
        LIMIT 1
    """

    try:
        rows = list(_search(query))
        if not rows:
            return {
                "campaign_id": cleaned_campaign_id,
                "date_range": date_range,
                "found": False,
                "metrics": None,
            }

        row = rows[0]
        cost_usd = row.metrics.cost_micros / 1_000_000
        avg_cpc_usd = row.metrics.average_cpc / 1_000_000
        return {
            "campaign_id": str(row.campaign.id),
            "campaign_name": row.campaign.name,
            "date_range": date_range,
            "found": True,
            "metrics": {
                "impressions": int(row.metrics.impressions),
                "clicks": int(row.metrics.clicks),
                "cost_usd": round(cost_usd, 2),
                "cost_micros": int(row.metrics.cost_micros),
                "cpc_usd": round(avg_cpc_usd, 2),
                "ctr": float(row.metrics.ctr),
            },
        }
    except Exception as exc:
        raise GoogleAdsToolError(_build_error_payload(exc)["message"]) from exc


@mcp.tool()
def gads_inject_keywords(
    ad_group_id: str,
    keywords: list[str],
    match_type: str,
) -> dict[str, Any]:
    """Create keyword criteria in an ad group."""
    cleaned_ad_group_id = _digits_only(ad_group_id)
    if not cleaned_ad_group_id:
        raise ValueError("ad_group_id must contain digits.")

    cleaned_keywords = [keyword.strip() for keyword in keywords if keyword.strip()]
    if not cleaned_keywords:
        raise ValueError("keywords must contain at least one non-empty term.")

    normalized_match_type = _validate_match_type(match_type)
    client = get_google_ads_client()
    ad_group_criterion_service = client.get_service("AdGroupCriterionService")
    ad_group_resource_name = _get_ad_group_resource_name(cleaned_ad_group_id)

    operations = []
    for keyword in cleaned_keywords:
        operation = client.get_type("AdGroupCriterionOperation")
        criterion = operation.create
        criterion.ad_group = ad_group_resource_name
        criterion.status = client.enums.AdGroupCriterionStatusEnum.ENABLED
        criterion.keyword.text = keyword
        criterion.keyword.match_type = getattr(
            client.enums.KeywordMatchTypeEnum,
            normalized_match_type,
        )
        operations.append(operation)

    try:
        response = ad_group_criterion_service.mutate_ad_group_criteria(
            customer_id=get_settings().customer_id,
            operations=operations,
        )
        criterion_ids = [_extract_resource_id(result.resource_name) for result in response.results]
        return {
            "success": True,
            "ad_group_id": cleaned_ad_group_id,
            "match_type": normalized_match_type,
            "criterion_ids": criterion_ids,
            "resource_names": [result.resource_name for result in response.results],
        }
    except Exception as exc:
        raise GoogleAdsToolError(_build_error_payload(exc)["message"]) from exc


@mcp.tool()
def gads_set_budget(campaign_id: str, new_budget_usd: float) -> dict[str, Any]:
    """Update a campaign's daily budget in USD."""
    cleaned_campaign_id = _digits_only(campaign_id)
    if not cleaned_campaign_id:
        raise ValueError("campaign_id must contain digits.")
    if new_budget_usd <= 0:
        raise ValueError("new_budget_usd must be greater than 0.")

    client = get_google_ads_client()
    campaign_budget_service = client.get_service("CampaignBudgetService")
    budget_resource_name = _lookup_campaign_budget_resource_name(cleaned_campaign_id)
    new_budget_micros = int(round(new_budget_usd * 1_000_000))

    operation = client.get_type("CampaignBudgetOperation")
    budget = operation.update
    budget.resource_name = budget_resource_name
    budget.amount_micros = new_budget_micros
    client.copy_from(operation.update_mask, protobuf_helpers.field_mask(None, budget._pb))

    try:
        response = campaign_budget_service.mutate_campaign_budgets(
            customer_id=get_settings().customer_id,
            operations=[operation],
        )
        return {
            "success": True,
            "campaign_id": cleaned_campaign_id,
            "budget_resource_name": response.results[0].resource_name,
            "new_budget_usd": round(new_budget_usd, 2),
            "new_budget_micros": new_budget_micros,
        }
    except Exception as exc:
        raise GoogleAdsToolError(_build_error_payload(exc)["message"]) from exc


@mcp.tool()
def gads_kill_switch(entity_id: str, entity_type: str) -> dict[str, Any]:
    """Pause a campaign or ad group immediately."""
    cleaned_entity_id = _digits_only(entity_id)
    if not cleaned_entity_id:
        raise ValueError("entity_id must contain digits.")

    normalized_entity_type = _validate_entity_type(entity_type)
    client = get_google_ads_client()

    try:
        if normalized_entity_type == "CAMPAIGN":
            service = client.get_service("CampaignService")
            operation = client.get_type("CampaignOperation")
            campaign = operation.update
            campaign.resource_name = _get_campaign_resource_name(cleaned_entity_id)
            campaign.status = client.enums.CampaignStatusEnum.PAUSED
            client.copy_from(
                operation.update_mask,
                protobuf_helpers.field_mask(None, campaign._pb),
            )
            response = service.mutate_campaigns(
                customer_id=get_settings().customer_id,
                operations=[operation],
            )
        else:
            service = client.get_service("AdGroupService")
            operation = client.get_type("AdGroupOperation")
            ad_group = operation.update
            ad_group.resource_name = _get_ad_group_resource_name(cleaned_entity_id)
            ad_group.status = client.enums.AdGroupStatusEnum.PAUSED
            client.copy_from(
                operation.update_mask,
                protobuf_helpers.field_mask(None, ad_group._pb),
            )
            response = service.mutate_ad_groups(
                customer_id=get_settings().customer_id,
                operations=[operation],
            )

        return {
            "success": True,
            "entity_id": cleaned_entity_id,
            "entity_type": normalized_entity_type,
            "status": "PAUSED",
            "resource_name": response.results[0].resource_name,
        }
    except Exception as exc:
        raise GoogleAdsToolError(_build_error_payload(exc)["message"]) from exc


@mcp.tool()
def gads_create_ad(
    ad_group_id: str,
    headlines: list[str],
    descriptions: list[str],
    final_url: str,
) -> dict[str, Any]:
    """Create and deploy a Responsive Search Ad."""
    cleaned_ad_group_id = _digits_only(ad_group_id)
    if not cleaned_ad_group_id:
        raise ValueError("ad_group_id must contain digits.")
    if not final_url.strip():
        raise ValueError("final_url must be a non-empty URL.")

    cleaned_headlines, cleaned_descriptions = _validate_assets(headlines, descriptions)
    client = get_google_ads_client()
    ad_group_ad_service = client.get_service("AdGroupAdService")

    operation = client.get_type("AdGroupAdOperation")
    ad_group_ad = operation.create
    ad_group_ad.status = client.enums.AdGroupAdStatusEnum.ENABLED
    ad_group_ad.ad_group = _get_ad_group_resource_name(cleaned_ad_group_id)
    ad_group_ad.ad.final_urls.append(final_url.strip())

    for headline in cleaned_headlines:
        asset = client.get_type("AdTextAsset")
        asset.text = headline
        ad_group_ad.ad.responsive_search_ad.headlines.append(asset)

    for description in cleaned_descriptions:
        asset = client.get_type("AdTextAsset")
        asset.text = description
        ad_group_ad.ad.responsive_search_ad.descriptions.append(asset)

    try:
        response = ad_group_ad_service.mutate_ad_group_ads(
            customer_id=get_settings().customer_id,
            operations=[operation],
        )
        resource_name = response.results[0].resource_name
        return {
            "success": True,
            "ad_group_id": cleaned_ad_group_id,
            "ad_id": _extract_resource_id(resource_name),
            "resource_name": resource_name,
            "status": "ENABLED",
        }
    except Exception as exc:
        raise GoogleAdsToolError(_build_error_payload(exc)["message"]) from exc


def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()
