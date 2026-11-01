#!/usr/bin/env bash
#
# Nuke Script - Complete cleanup of Talos test clusters
#
# This script removes:
# - All talos9s Docker containers
# - All talos9s Docker networks
# - All talos9s talosconfig contexts
# - The talos clusters state directory
# - Generated output files
#
# Usage:
#   ./nuke.sh          # Clean up everything
#   ./nuke.sh --help   # Show help
#

set -euo pipefail

# Configuration
CLUSTER_NAME="${TALOS_CLUSTER_NAME:-talos9s}"

# Paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT_DIR="${SCRIPT_DIR}/../output"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() { echo -e "${BLUE}[INFO]${NC} $*"; }
log_success() { echo -e "${GREEN}[OK]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

show_help() {
    cat << 'EOF'
Nuke Script - Complete cleanup of Talos test clusters

USAGE:
    ./nuke.sh [OPTIONS]

OPTIONS:
    --help, -h      Show this help message
    --dry-run       Show what would be deleted without actually deleting

WHAT GETS CLEANED:
    - Docker containers matching 'talos9s*'
    - Docker networks matching 'talos9s*'
    - All talosconfig contexts matching 'talos9s*'
    - ~/.talos/clusters/talos9s directory
    - test-clusters/output/ generated files

EOF
}

nuke_docker() {
    log_info "Cleaning up Docker containers..."

    # Find and remove talos9s containers
    local containers
    containers=$(docker ps -a --filter "name=${CLUSTER_NAME}" -q 2>/dev/null || true)

    if [[ -n "${containers}" ]]; then
        if [[ "${DRY_RUN:-false}" == "true" ]]; then
            echo "  Would remove containers:"
            docker ps -a --filter "name=${CLUSTER_NAME}" --format "    {{.Names}}"
        else
            docker rm -f ${containers} 2>/dev/null || true
            log_success "Removed Docker containers"
        fi
    else
        log_info "No Docker containers to remove"
    fi

    log_info "Cleaning up Docker networks..."

    # Find and remove talos9s networks
    local networks
    networks=$(docker network ls --filter "name=${CLUSTER_NAME}" -q 2>/dev/null || true)

    if [[ -n "${networks}" ]]; then
        if [[ "${DRY_RUN:-false}" == "true" ]]; then
            echo "  Would remove networks:"
            docker network ls --filter "name=${CLUSTER_NAME}" --format "    {{.Name}}"
        else
            for net in ${networks}; do
                docker network rm "${net}" 2>/dev/null || true
            done
            log_success "Removed Docker networks"
        fi
    else
        log_info "No Docker networks to remove"
    fi
}

nuke_talosconfig() {
    log_info "Cleaning up talosconfig contexts..."

    # Get all talos9s contexts
    local contexts
    contexts=$(talosctl config contexts 2>/dev/null | grep "${CLUSTER_NAME}" | awk '{print $1}' | grep -v "^CURRENT$" || true)

    # Also check for the current context marker
    local current_context
    current_context=$(talosctl config contexts 2>/dev/null | grep "^\*" | awk '{print $2}' || true)

    if [[ -n "${contexts}" ]]; then
        if [[ "${DRY_RUN:-false}" == "true" ]]; then
            echo "  Would remove contexts:"
            for ctx in ${contexts}; do
                echo "    ${ctx}"
            done
        else
            for ctx in ${contexts}; do
                talosctl config remove "${ctx}" -y 2>/dev/null && echo "  Removed: ${ctx}" || true
            done
            log_success "Removed talosconfig contexts"
        fi
    else
        log_info "No talosconfig contexts to remove"
    fi

    # Clean up clusters state directory
    local clusters_dir="${HOME}/.talos/clusters/${CLUSTER_NAME}"
    if [[ -d "${clusters_dir}" ]]; then
        if [[ "${DRY_RUN:-false}" == "true" ]]; then
            echo "  Would remove directory: ${clusters_dir}"
        else
            rm -rf "${clusters_dir}"
            log_success "Removed clusters state directory"
        fi
    fi
}

nuke_output() {
    log_info "Cleaning up output files..."

    if [[ -d "${OUTPUT_DIR}" ]]; then
        local files
        files=$(find "${OUTPUT_DIR}" -type f ! -name ".gitkeep" 2>/dev/null || true)

        if [[ -n "${files}" ]]; then
            if [[ "${DRY_RUN:-false}" == "true" ]]; then
                echo "  Would remove files:"
                echo "${files}" | while read -r f; do
                    echo "    ${f}"
                done
            else
                find "${OUTPUT_DIR}" -type f ! -name ".gitkeep" -delete 2>/dev/null || true
                log_success "Removed output files"
            fi
        else
            log_info "No output files to remove"
        fi
    fi
}

nuke_talos_cluster() {
    log_info "Destroying Talos cluster..."

    if [[ "${DRY_RUN:-false}" == "true" ]]; then
        echo "  Would run: talosctl cluster destroy --name ${CLUSTER_NAME}"
    else
        talosctl cluster destroy --name "${CLUSTER_NAME}" 2>/dev/null || true
    fi
}

main() {
    local DRY_RUN=false

    # Parse arguments
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --help|-h)
                show_help
                exit 0
                ;;
            --dry-run)
                DRY_RUN=true
                shift
                ;;
            *)
                log_error "Unknown option: $1"
                show_help
                exit 1
                ;;
        esac
    done

    export DRY_RUN

    echo ""
    if [[ "${DRY_RUN}" == "true" ]]; then
        log_warn "DRY RUN MODE - No changes will be made"
    else
        log_warn "This will completely clean up all ${CLUSTER_NAME} resources!"
    fi
    echo ""

    # Run cleanup in order
    nuke_talos_cluster
    nuke_docker
    nuke_talosconfig
    nuke_output

    echo ""
    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "Dry run complete. Use without --dry-run to actually clean up."
    else
        log_success "Nuke complete! All ${CLUSTER_NAME} resources have been removed."
        echo ""
        echo "You can now create a fresh cluster with:"
        echo "  ./scripts/cluster.sh create <profile>"
    fi
}

main "$@"
