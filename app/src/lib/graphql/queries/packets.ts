import { graphql } from "gql.tada"
import { packetListDataFragment } from "$lib/graphql/fragments/packets"

export const packetsLatestQuery = graphql(
  /* GraphQL */ `
    query PacketsLatestQuery($limit: Int = 100) {
      v0_packets(limit: $limit, order_by: { source_time: desc_nulls_last }) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsTimestampQuery = graphql(
  /* GraphQL */ `
  query PacketsTimestampQuery($limit: Int! = 100, $timestamp: timestamptz!)
    @cached(ttl: 1000) {
      newer: v0_packets(
        limit: $limit
        order_by: [{ source_time: asc }, { destination_time: asc }]
        where: { source_time: { _gte: $timestamp } }
      ) {
        ...PacketListData
      }
      older: v0_packets(
        limit: $limit
        order_by: [
          { source_time: desc }
          { destination_time: desc }
        ]
        where: { source_time: { _lt: $timestamp } }
      ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsByChainLatestQuery = graphql(
  /* GraphQL */ `
    query PacketsByChainLatestQuery($limit: Int, $chain_id: String!) {
      v0_packets(
        limit: $limit 
        order_by: { source_time: desc_nulls_last }
        where: { _or: [
          { from_chain_id: { _eq: $chain_id }}
          { to_chain_id: { _eq: $chain_id }}
        ]}
        ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsByChainTimestampQuery = graphql(
  /* GraphQL */ `
    query PacketsByChainTimestampQuery($limit: Int!, $chain_id: String!, $timestamp: timestamptz!) @cached(ttl: 1000) {
      newer: v0_packets(
        limit: $limit
        order_by: [{ source_time: asc }, { destination_time: asc }]
        where: {
          _and: [
            { source_time: { _gte: $timestamp } }
            {
              _or: [
                { from_chain_id: { _eq: $chain_id }}
                { to_chain_id: { _eq: $chain_id }}
              ]
            }
          ]
        }

      ) {
        ...PacketListData
      }
      older: v0_packets(
        limit: $limit
        order_by: [ { source_time: desc } { destination_time: desc } ]
        where: {
          _and: [
            { source_time: { _lt: $timestamp } }
            {
              _or: [
                { from_chain_id: { _eq: $chain_id }}
                { to_chain_id: { _eq: $chain_id }}
              ]
            }
          ]
        }
      ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsByConnectionIdLatestQuery = graphql(
  /* GraphQL */ `
    query PacketsByConnectionIdLatestQuery($limit: Int!, $chain_id: String!, $connection_id: String!) {
      v0_packets(
        limit: $limit 
        order_by: { source_time: desc_nulls_last }
        where: { 
          _or: [
            { _and: [{from_chain_id: { _eq: $chain_id }} {from_connection_id: { _eq: $connection_id }}] }
            { _and: [{to_chain_id: { _eq: $chain_id }} {to_connection_id: { _eq: $connection_id }}] }
          ]
        }
        ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsByConnectionIdTimestampQuery = graphql(
  /* GraphQL */ `
    query PacketsByConnectionIdTimestampQuery($limit: Int!, $chain_id: String!, $connection_id: String!, $timestamp: timestamptz!) @cached(ttl: 1000) {
      newer: v0_packets(
        limit: $limit
        order_by: [{ source_time: asc }, { destination_time: asc }]
        where: {
          _and: [
            { source_time: { _gte: $timestamp } }
            {
              _or: [
                { _and: [{from_chain_id: { _eq: $chain_id }} {from_connection_id: { _eq: $connection_id }}] }
                { _and: [{to_chain_id: { _eq: $chain_id }} {to_connection_id: { _eq: $connection_id }}] }
              ]
            }
          ]
        }

      ) {
        ...PacketListData
      }
      older: v0_packets(
        limit: $limit
        order_by: [ { source_time: desc } { destination_time: desc } ]
        where: {
          _and: [
            { source_time: { _lt: $timestamp } }
            {
              _or: [
                { _and: [{from_chain_id: { _eq: $chain_id }} {from_connection_id: { _eq: $connection_id }}] }
                { _and: [{to_chain_id: { _eq: $chain_id }} {to_connection_id: { _eq: $connection_id }}] }
              ]
            }
          ]
        }
      ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsByChannelIdLatestQuery = graphql(
  /* GraphQL */ `
    query PacketsByChannelIdLatestQuery($limit: Int!, $chain_id: String!, $connection_id: String!, $channel_id: String!) {
      v0_packets(
        limit: $limit 
        order_by: { source_time: desc_nulls_last }
        where: { 
          _or: [
            { _and: [{from_chain_id: { _eq: $chain_id }} {from_connection_id: { _eq: $connection_id }} {from_channel_id: { _eq: $channel_id }}] }
            { _and: [{to_chain_id: { _eq: $chain_id }} {to_connection_id: { _eq: $connection_id }} {to_channel_id: { _eq: $channel_id }}] }
          ]
        }
        ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)

export const packetsByChannelIdTimestampQuery = graphql(
  /* GraphQL */ `
    query PacketsByChannelIdTimestampQuery($limit: Int!, $chain_id: String!, $connection_id: String!, $channel_id: String!,  $timestamp: timestamptz!) @cached(ttl: 1000) {
      newer: v0_packets(
        limit: $limit
        order_by: [{ source_time: asc }, { destination_time: asc }]
        where: {
          _and: [
            { source_time: { _gte: $timestamp } }
            {
              _or: [
                { _and: [{from_chain_id: { _eq: $chain_id }} {from_connection_id: { _eq: $connection_id }} {from_channel_id: { _eq: $channel_id }}] }
                { _and: [{to_chain_id: { _eq: $chain_id }} {to_connection_id: { _eq: $connection_id }} {to_channel_id: { _eq: $channel_id }}] }
              ]
            }
          ]
        }

      ) {
        ...PacketListData
      }
      older: v0_packets(
        limit: $limit
        order_by: [ { source_time: desc } { destination_time: desc } ]
        where: {
          _and: [
            { source_time: { _lt: $timestamp } }
            {
              _or: [
                { _and: [{from_chain_id: { _eq: $chain_id }} {from_connection_id: { _eq: $connection_id }} {from_channel_id: { _eq: $channel_id }}] }
                { _and: [{to_chain_id: { _eq: $chain_id }} {to_connection_id: { _eq: $connection_id }} {to_channel_id: { _eq: $channel_id }}] }
              ]
            }
          ]
        }
      ) {
        ...PacketListData
      }
    }
  `,
  [packetListDataFragment]
)