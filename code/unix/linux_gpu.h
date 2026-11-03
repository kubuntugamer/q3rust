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
// linux_gpu.h -- multi-GPU enumeration and selection

#ifndef __LINUX_GPU_H__
#define __LINUX_GPU_H__

// Enumerates DRM render devices, registers r_gpuIndex and the gpu_list
// command, and applies the selected device to the GL environment. Must run
// before the GL context is created.
void GL_GPUInit( void );

// Logs which GPU the driver actually bound to, next to what was requested.
void GL_GPUReport( const char *vendor, const char *renderer );

#endif // __LINUX_GPU_H__
