/*
===========================================================================
Copyright (C) 1999-2005 Id Software, Inc.

This file is part of Quake III Arena source code.

Quake III Arena source code is free software; you can redistribute it
and/or modify it under the terms of the GNU General Public License as
published by the Free Software Foundation; either version 2 of the License,
or (at your option) any later version.

Quake III Arena source code is distributed in the hope that it will be
useful, but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with Foobar; if not, write to the Free Software
Foundation, Inc., 51 Franklin St, Fifth Floor, Boston, MA  02110-1301  USA
===========================================================================
*/
// linux_gpu.c -- multi-GPU enumeration and render-device selection
//
// The renderer is GLX, so the GPU a context lands on is decided by the GL
// driver, not by us. What this file can do is tell the driver which GPU we
// want before the context exists:
//
//   Mesa (radeonsi/iris/nouveau/llvmpipe): DRI_PRIME=pci-<pci-address>
//   NVIDIA proprietary:                    __NV_PRIME_RENDER_OFFLOAD=1 plus
//                                          __GLX_VENDOR_LIBRARY_NAME=nvidia
//
// Both are read when the context is created, which is why the selection is
// applied in GL_GPUInit() before GLW_LoadOpenGL() and why changing
// r_gpuIndex takes effect on the next vid_restart.
//
// The available GPUs come from the DRM sysfs nodes, so this works without
// adding an EGL or XRandR dependency.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <dirent.h>
#include <sys/types.h>

#include "../game/q_shared.h"
#include "../qcommon/qcommon.h"
#include "linux_gpu.h"

#define MAX_GPUS 8

typedef struct {
	char		card[32];		// card0, card1, ...
	char		pci[32];		// 0000:01:00.0
	unsigned	vendor;
	unsigned	device;
	qboolean	bootVga;		// firmware's primary display device
} gpuInfo_t;

static cvar_t	*r_gpuIndex;
static gpuInfo_t gpus[MAX_GPUS];
static int		numGpus;

/*
==============
GPU_VendorName
==============
*/
static const char *GPU_VendorName( unsigned vendor )
{
	switch ( vendor )
	{
		case 0x10de: return "NVIDIA";
		case 0x1002: return "AMD";
		case 0x1022: return "AMD";
		case 0x8086: return "Intel";
		case 0x13b5: return "ARM";
		case 0x5143: return "Qualcomm";
		case 0x1af4: return "virtio";
		case 0x15ad: return "VMware";
		case 0x1234: return "QEMU";
		case 0x1414: return "Microsoft";
		default:     return "unknown";
	}
}

/*
==============
GPU_ReadSysfsUInt

Reads a 0x-prefixed or decimal value from a one-line sysfs attribute.
==============
*/
static qboolean GPU_ReadSysfsUInt( const char *path, unsigned *out )
{
	FILE	*f;
	unsigned value;

	f = fopen( path, "r" );
	if ( !f )
	{
		return qfalse;
	}
	if ( fscanf( f, "%i", &value ) != 1 )
	{
		fclose( f );
		return qfalse;
	}
	fclose( f );

	*out = value;
	return qtrue;
}

/*
==============
GPU_ReadPciAddress

/sys/class/drm/cardN/device is a symlink into the PCI hierarchy; the last
path component is the device's PCI address.
==============
*/
static void GPU_ReadPciAddress( const char *card, char *out, int outSize )
{
	char		link[512];
	char		resolved[1024];
	const char	*base;
	ssize_t		len;

	out[0] = 0;

	Com_sprintf( link, sizeof( link ), "/sys/class/drm/%s/device", card );
	len = readlink( link, resolved, sizeof( resolved ) - 1 );
	if ( len <= 0 )
	{
		return;
	}
	resolved[len] = 0;

	base = strrchr( resolved, '/' );
	Q_strncpyz( out, base ? base + 1 : resolved, outSize );
}

/*
==============
GPU_Scan

Fills gpus[] with the DRM cards that expose a PCI device. Connector entries
(card0-DP-1) and non-PCI cards are skipped.
==============
*/
static int GPU_Scan( void )
{
	DIR				*dir;
	struct dirent	*entry;
	char			path[512];
	unsigned		vendor, device, boot;
	int				i;

	numGpus = 0;

	dir = opendir( "/sys/class/drm" );
	if ( !dir )
	{
		return 0;
	}

	while ( ( entry = readdir( dir ) ) != NULL && numGpus < MAX_GPUS )
	{
		const char *name = entry->d_name;

		if ( strncmp( name, "card", 4 ) != 0 )
		{
			continue;
		}
		// card0 is a GPU, card0-DP-1 is a connector off it
		for ( i = 4; name[i]; i++ )
		{
			if ( name[i] < '0' || name[i] > '9' )
			{
				break;
			}
		}
		if ( name[i] )
		{
			continue;
		}

		Com_sprintf( path, sizeof( path ), "/sys/class/drm/%s/device/vendor", name );
		if ( !GPU_ReadSysfsUInt( path, &vendor ) )
		{
			continue;	// not a PCI device (platform/firmware framebuffer)
		}

		device = 0;
		Com_sprintf( path, sizeof( path ), "/sys/class/drm/%s/device/device", name );
		GPU_ReadSysfsUInt( path, &device );

		boot = 0;
		Com_sprintf( path, sizeof( path ), "/sys/class/drm/%s/device/boot_vga", name );
		GPU_ReadSysfsUInt( path, &boot );

		Q_strncpyz( gpus[numGpus].card, name, sizeof( gpus[numGpus].card ) );
		GPU_ReadPciAddress( name, gpus[numGpus].pci, sizeof( gpus[numGpus].pci ) );
		gpus[numGpus].vendor = vendor;
		gpus[numGpus].device = device;
		gpus[numGpus].bootVga = ( boot == 1 ) ? qtrue : qfalse;

		// Only devices that can render are useful; i915/amdgpu/nvidia all
		// expose renderD* siblings. Cards without one (e.g. a KMS-only node)
		// are still listed, since DRM render nodes are not guaranteed to be
		// named per card on every kernel.
		numGpus++;
	}

	closedir( dir );
	return numGpus;
}

/*
==============
GPU_PrintList_f
==============
*/
static void GPU_PrintList_f( void )
{
	int i;

	if ( numGpus == 0 )
	{
		Com_Printf( "No DRM GPUs found under /sys/class/drm\n" );
		return;
	}

	Com_Printf( "----- GPUs -----\n" );
	for ( i = 0; i < numGpus; i++ )
	{
		Com_Printf( "%2d: %-8s %04x:%04x %-32s%s%s\n",
			i,
			GPU_VendorName( gpus[i].vendor ),
			gpus[i].vendor, gpus[i].device,
			gpus[i].pci,
			gpus[i].bootVga ? " primary" : "",
			( i == r_gpuIndex->integer ) ? "  <- r_gpuIndex" : "" );
	}
	Com_Printf( "----------------\n" );
	Com_Printf( "r_gpuIndex %d (%s); changing it needs a vid_restart\n",
		r_gpuIndex->integer,
		r_gpuIndex->integer < 0 ? "auto" : "explicit" );
}

/*
==============
GPU_ApplySelection

Tells the GL driver which GPU to use for the context it is about to create.
==============
*/
static void GPU_ApplySelection( void )
{
	const gpuInfo_t	*gpu;
	char			prime[64];
	int				index;

	index = r_gpuIndex->integer;

	if ( index < 0 )
	{
		return;		// auto: leave the driver's default choice alone
	}

	if ( index >= numGpus )
	{
		Com_Printf( "r_gpuIndex %d is out of range (%d GPU(s) found), using the default device\n",
			index, numGpus );
		return;
	}

	gpu = &gpus[index];

	Com_sprintf( prime, sizeof( prime ), "pci-%s", gpu->pci );
	setenv( "DRI_PRIME", prime, 1 );

	if ( gpu->vendor == 0x10de )
	{
		// PRIME render offload for the proprietary driver
		setenv( "__NV_PRIME_RENDER_OFFLOAD", "1", 1 );
		setenv( "__GLX_VENDOR_LIBRARY_NAME", "nvidia", 1 );
	}
	else
	{
		unsetenv( "__NV_PRIME_RENDER_OFFLOAD" );
		unsetenv( "__GLX_VENDOR_LIBRARY_NAME" );
	}

	Com_Printf( "Multi-GPU: requesting %s %04x:%04x at %s (DRI_PRIME=%s)\n",
		GPU_VendorName( gpu->vendor ), gpu->vendor, gpu->device,
		gpu->pci, prime );
}

/*
==============
GL_GPUInit
==============
*/
void GL_GPUInit( void )
{
	r_gpuIndex = Cvar_Get( "r_gpuIndex", "-1", CVAR_ARCHIVE | CVAR_LATCH );

	Cmd_AddCommand( "gpu_list", GPU_PrintList_f );

	GPU_Scan();

	if ( numGpus > 1 )
	{
		Com_Printf( "Multi-GPU: %d GPUs detected, use gpu_list to inspect them\n", numGpus );
	}

	GPU_ApplySelection();
}

/*
==============
GL_GPUReport
==============
*/
void GL_GPUReport( const char *vendor, const char *renderer )
{
	Com_Printf( "GL_VENDOR: %s\n", vendor ? vendor : "" );
	Com_Printf( "GL_RENDERER: %s\n", renderer ? renderer : "" );

	if ( r_gpuIndex->integer < 0 || r_gpuIndex->integer >= numGpus )
	{
		return;		// nothing was requested, nothing to check
	}

	// The driver does not have to honour the request (a single-GPU system, a
	// compositor-owned context, an unsupported PRIME setup). Say so plainly
	// rather than pretending the selection took effect.
	Com_Printf( "Multi-GPU: requested GPU %d (%s %s); the driver reports the renderer above\n",
		r_gpuIndex->integer,
		GPU_VendorName( gpus[r_gpuIndex->integer].vendor ),
		gpus[r_gpuIndex->integer].pci );
}
